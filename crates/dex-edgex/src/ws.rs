use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use config::EdgeXConfig;
use core_types::{now_wall_ms, Dex, MessageTrace, OrderBook, Symbol};
use dex_traits::{
    Backoff, ConnectionState, ConnectionStatus, MarketDataError, MarketDataSource, SourceMetrics,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

use crate::book_builder::BookBuilder;
use crate::message::{
    depth_channel, ping_message, subscribe_message, unsubscribe_message, DataType, EdgeXEnvelope,
    EdgeXMessageKind,
};
use crate::meta::fetch_contract_ids;

/// メッセージ処理後に接続へ返すべき操作。
///
/// 送信は WS の write 半分を持つ受信ループ側で行い、パース処理と I/O を分ける。
#[derive(Debug, PartialEq, Eq)]
enum Reaction {
    None,
    Pong,
    /// 板を作り直すため、この channel を購読し直す。
    Resubscribe(String),
}

/// edgeX の板ストリーム。
pub struct EdgeXMarketData {
    cfg: EdgeXConfig,
    state: Arc<ConnectionState>,
    messages_received: Arc<AtomicU64>,
    books_emitted: Arc<AtomicU64>,
    parse_errors: Arc<AtomicU64>,
    sequence_gaps: Arc<AtomicU64>,
}

impl EdgeXMarketData {
    pub fn new(cfg: EdgeXConfig) -> Self {
        EdgeXMarketData {
            cfg,
            state: Arc::new(ConnectionState::new()),
            messages_received: Arc::new(AtomicU64::new(0)),
            books_emitted: Arc::new(AtomicU64::new(0)),
            parse_errors: Arc::new(AtomicU64::new(0)),
            sequence_gaps: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn messages_received(&self) -> u64 {
        self.messages_received.load(Ordering::Relaxed)
    }

    pub fn books_emitted(&self) -> u64 {
        self.books_emitted.load(Ordering::Relaxed)
    }

    pub fn parse_errors(&self) -> u64 {
        self.parse_errors.load(Ordering::Relaxed)
    }

    /// シーケンス欠損の検知回数。増え続ける場合は差分再構築か接続品質の問題。
    pub fn sequence_gaps(&self) -> u64 {
        self.sequence_gaps.load(Ordering::Relaxed)
    }

    /// 銘柄 → contractId を解決する。
    ///
    /// REST メタデータを引いたうえで、設定ファイルの `contract_ids` で上書きする
    /// （手動指定を常に優先させ、API 側の変更で詰まっても回避できるようにする）。
    async fn resolve_contract_ids(
        &self,
        symbols: &[Symbol],
    ) -> Result<BTreeMap<Symbol, String>, MarketDataError> {
        let mut resolved = BTreeMap::new();

        if self.cfg.resolve_contract_ids {
            match fetch_contract_ids(&self.cfg.metadata_url).await {
                Ok(ids) => {
                    info!(dex = %Dex::EdgeX, count = ids.len(), "メタデータから contractId を解決");
                    resolved.extend(ids);
                }
                Err(e) => {
                    warn!(
                        dex = %Dex::EdgeX,
                        error = %e,
                        "メタデータ取得に失敗。設定ファイルの contract_ids にフォールバック"
                    );
                }
            }
        }
        for (symbol, id) in &self.cfg.contract_ids {
            resolved.insert(*symbol, id.clone());
        }

        let selected: BTreeMap<Symbol, String> = symbols
            .iter()
            .filter_map(|s| resolved.get(s).map(|id| (*s, id.clone())))
            .collect();

        for symbol in symbols {
            if !selected.contains_key(symbol) {
                warn!(
                    dex = %Dex::EdgeX,
                    symbol = %symbol,
                    "contractId を解決できないため購読をスキップします（config の [dex.edgex.contract_ids] に追記してください）"
                );
            }
        }

        if selected.is_empty() {
            return Err(MarketDataError::Config(
                "購読可能な contractId が 1 つもありません".to_string(),
            ));
        }
        Ok(selected)
    }

    /// 1 回の接続セッション。
    async fn session(
        &self,
        contracts: &BTreeMap<Symbol, String>,
        depth: usize,
        tx: &mpsc::Sender<OrderBook>,
        backoff: &mut Backoff,
    ) -> Result<(), MarketDataError> {
        let (ws, _resp) = tokio_tungstenite::connect_async(self.cfg.ws_url.as_str())
            .await
            .map_err(|e| MarketDataError::WebSocket(e.to_string()))?;

        self.state.set_connected();
        backoff.reset();
        info!(dex = %Dex::EdgeX, url = %self.cfg.ws_url, "WS 接続確立");

        let (mut write, mut read) = ws.split();

        // contractId → (Symbol, ローカル板)。セッションごとに作り直す。
        let mut builders: BTreeMap<String, BookBuilder> = BTreeMap::new();
        let mut channels: Vec<String> = Vec::with_capacity(contracts.len());
        for (symbol, contract_id) in contracts {
            builders.insert(
                contract_id.clone(),
                BookBuilder::new(*symbol, self.cfg.depth_level),
            );
            let channel = depth_channel(contract_id, self.cfg.depth_level);
            write
                .send(Message::text(subscribe_message(&channel)))
                .await
                .map_err(|e| MarketDataError::Subscribe(e.to_string()))?;
            debug!(dex = %Dex::EdgeX, symbol = %symbol, channel = %channel, "depth を購読");
            channels.push(channel);
        }

        let mut ping_ticker =
            tokio::time::interval(Duration::from_secs(self.cfg.ping_interval_secs.max(1)));
        ping_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ping_ticker.tick().await;

        // 定期再購読。差分再構築のズレをスナップショットで突き合わせるための仕組み。
        let resync_secs = if self.cfg.resync_interval_secs == 0 {
            u64::MAX / 2 // 実質無効（tokio::time::interval は 0 を許容しない）
        } else {
            self.cfg.resync_interval_secs
        };
        let mut resync_ticker = tokio::time::interval(Duration::from_secs(resync_secs));
        resync_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        resync_ticker.tick().await;

        loop {
            tokio::select! {
                incoming = read.next() => {
                    let Some(msg) = incoming else {
                        warn!(dex = %Dex::EdgeX, "WS ストリームが終了");
                        return Ok(());
                    };
                    let msg = msg.map_err(|e| MarketDataError::WebSocket(e.to_string()))?;
                    match msg {
                        Message::Text(text) => {
                            // パース前に受信時刻を確定させる（レイテンシ計測の起点）
                            let trace = MessageTrace::on_receive();
                            self.messages_received.fetch_add(1, Ordering::Relaxed);
                            let reaction = self
                                .handle_text(text.as_str(), depth, trace, &mut builders, tx)
                                .await?;
                            match reaction {
                                Reaction::None => {}
                                Reaction::Pong => {
                                    write
                                        .send(Message::text(format!(
                                            r#"{{"type":"pong","time":"{}"}}"#,
                                            now_wall_ms()
                                        )))
                                        .await
                                        .map_err(|e| MarketDataError::WebSocket(e.to_string()))?;
                                }
                                Reaction::Resubscribe(channel) => {
                                    resubscribe(&mut write, &channel).await?;
                                }
                            }
                        }
                        Message::Ping(payload) => {
                            write
                                .send(Message::Pong(payload))
                                .await
                                .map_err(|e| MarketDataError::WebSocket(e.to_string()))?;
                        }
                        Message::Close(frame) => {
                            warn!(dex = %Dex::EdgeX, ?frame, "WS クローズを受信");
                            return Ok(());
                        }
                        Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {}
                    }
                }
                _ = ping_ticker.tick() => {
                    if let Err(e) = write.send(Message::text(ping_message(now_wall_ms()))).await {
                        warn!(dex = %Dex::EdgeX, error = %e, "ping 送信に失敗");
                        return Err(MarketDataError::WebSocket(e.to_string()));
                    }
                }
                _ = resync_ticker.tick() => {
                    info!(dex = %Dex::EdgeX, channels = channels.len(), "整合性チェックのため再購読");
                    for channel in &channels {
                        resubscribe(&mut write, channel).await?;
                    }
                }
            }
        }
    }

    /// テキストメッセージ 1 件を処理する。
    async fn handle_text(
        &self,
        text: &str,
        depth: usize,
        trace: MessageTrace,
        builders: &mut BTreeMap<String, BookBuilder>,
        tx: &mpsc::Sender<OrderBook>,
    ) -> Result<Reaction, MarketDataError> {
        let env: EdgeXEnvelope = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                self.parse_errors.fetch_add(1, Ordering::Relaxed);
                warn!(
                    dex = %Dex::EdgeX,
                    error = %e,
                    snippet = %text.chars().take(200).collect::<String>(),
                    "メッセージのパースに失敗"
                );
                return Ok(Reaction::None);
            }
        };

        match env.message_kind() {
            EdgeXMessageKind::Ping => return Ok(Reaction::Pong),
            EdgeXMessageKind::Pong | EdgeXMessageKind::Control => return Ok(Reaction::None),
            EdgeXMessageKind::Error => {
                error!(dex = %Dex::EdgeX, message = %text.chars().take(300).collect::<String>(), "取引所からエラー応答");
                return Ok(Reaction::None);
            }
            EdgeXMessageKind::Unknown => {
                debug!(dex = %Dex::EdgeX, snippet = %text.chars().take(200).collect::<String>(), "未知のメッセージ形式");
                return Ok(Reaction::None);
            }
            EdgeXMessageKind::Depth => {}
        }

        let data_type = env.data_type();
        let exchange_ts = env.exchange_ts_ms();
        let channel = env.channel.clone();
        let Some(depth_data) = env.depth() else {
            return Ok(Reaction::None);
        };

        // contractId はペイロード優先、無ければ channel 名（depth.<id>.<level>）から取る。
        let contract_id = depth_data
            .contract_id
            .clone()
            .or_else(|| channel.as_deref().and_then(contract_id_from_channel));
        let Some(contract_id) = contract_id else {
            debug!(dex = %Dex::EdgeX, "contractId を特定できないメッセージを無視");
            return Ok(Reaction::None);
        };

        let level = self.cfg.depth_level;
        let Some(builder) = builders.get_mut(&contract_id) else {
            debug!(dex = %Dex::EdgeX, contract_id = %contract_id, "未購読の contractId を無視");
            return Ok(Reaction::None);
        };

        match data_type {
            DataType::Snapshot => {
                let check = builder.apply_snapshot(depth_data);
                if check.was_initialized && check.mismatched_levels > 0 {
                    // 差分再構築のバグ・取りこぼしの兆候。板はスナップショットで是正済み。
                    warn!(
                        dex = %Dex::EdgeX,
                        symbol = %builder.symbol(),
                        mismatched_levels = check.mismatched_levels,
                        "ローカル板がスナップショットとズレていました"
                    );
                }
            }
            DataType::Changed => {
                if let Err(e) = builder.apply_diff(depth_data) {
                    self.sequence_gaps.fetch_add(1, Ordering::Relaxed);
                    warn!(
                        dex = %Dex::EdgeX,
                        symbol = %builder.symbol(),
                        error = %e,
                        "差分の適用に失敗。板を作り直します"
                    );
                    builder.reset();
                    let channel = channel.unwrap_or_else(|| depth_channel(&contract_id, level));
                    return Ok(Reaction::Resubscribe(channel));
                }
            }
            DataType::Unknown => {
                if builder.is_initialized() {
                    // 種別不明の更新を誤って適用すると板が静かに壊れる。作り直す方が安全。
                    warn!(
                        dex = %Dex::EdgeX,
                        symbol = %builder.symbol(),
                        "dataType 不明の更新。板を作り直します"
                    );
                    builder.reset();
                    let channel = channel.unwrap_or_else(|| depth_channel(&contract_id, level));
                    return Ok(Reaction::Resubscribe(channel));
                }
                // 未初期化なら全量として扱う
                builder.apply_snapshot(depth_data);
            }
        }

        let book = builder.to_order_book(depth, exchange_ts, trace);
        if let Err(e) = book.validate() {
            // 正規化の契約違反。上位に流すと分析データが汚れるため捨てる。
            warn!(dex = %Dex::EdgeX, symbol = %book.symbol, error = %e, "板の整合性チェックに失敗");
            return Ok(Reaction::None);
        }

        if tx.capacity() == 0 {
            warn!(dex = %Dex::EdgeX, "板チャネルが飽和（下流の処理が追いついていない）");
        }
        tx.send(book)
            .await
            .map_err(|_| MarketDataError::ChannelClosed)?;
        self.books_emitted.fetch_add(1, Ordering::Relaxed);
        Ok(Reaction::None)
    }
}

async fn resubscribe<S>(write: &mut S, channel: &str) -> Result<(), MarketDataError>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::fmt::Display,
{
    write
        .send(Message::text(unsubscribe_message(channel)))
        .await
        .map_err(|e| MarketDataError::WebSocket(e.to_string()))?;
    write
        .send(Message::text(subscribe_message(channel)))
        .await
        .map_err(|e| MarketDataError::WebSocket(e.to_string()))?;
    Ok(())
}

/// `depth.10000001.15` → `10000001`
fn contract_id_from_channel(channel: &str) -> Option<String> {
    let mut parts = channel.split('.');
    if parts.next()? != "depth" {
        return None;
    }
    parts.next().map(|s| s.to_string())
}

#[async_trait]
impl MarketDataSource for EdgeXMarketData {
    fn dex(&self) -> Dex {
        Dex::EdgeX
    }

    async fn subscribe_orderbooks(
        &self,
        symbols: &[Symbol],
        depth: usize,
        tx: mpsc::Sender<OrderBook>,
    ) -> Result<(), MarketDataError> {
        let contracts = self.resolve_contract_ids(symbols).await?;
        let policy = self.cfg.reconnect_policy();
        let mut backoff = Backoff::new(
            policy.base_delay_ms,
            policy.max_delay_ms,
            policy.max_attempts,
        );

        loop {
            match self.session(&contracts, depth, &tx, &mut backoff).await {
                Ok(()) => {
                    warn!(dex = %Dex::EdgeX, "セッションが切断されました");
                }
                Err(MarketDataError::ChannelClosed) => {
                    self.state.set_disconnected();
                    info!(dex = %Dex::EdgeX, "下流チャネルが閉じたため購読を終了");
                    return Err(MarketDataError::ChannelClosed);
                }
                Err(e) => {
                    warn!(dex = %Dex::EdgeX, error = %e, "セッションが異常終了");
                }
            }

            match backoff.next_delay() {
                Some(delay) => {
                    self.state.set_reconnecting(backoff.attempt());
                    info!(
                        dex = %Dex::EdgeX,
                        attempt = backoff.attempt(),
                        delay_ms = delay.as_millis() as u64,
                        "再接続を待機"
                    );
                    tokio::time::sleep(delay).await;
                }
                None => {
                    self.state.set_disconnected();
                    error!(dex = %Dex::EdgeX, attempts = backoff.attempt(), "再接続の上限に到達");
                    return Err(MarketDataError::ReconnectExhausted {
                        attempts: backoff.attempt(),
                    });
                }
            }
        }
    }

    fn connection_status(&self) -> ConnectionStatus {
        self.state.get()
    }

    fn metrics(&self) -> SourceMetrics {
        SourceMetrics {
            messages_received: self.messages_received(),
            books_emitted: self.books_emitted(),
            parse_errors: self.parse_errors(),
            sequence_gaps: self.sequence_gaps(),
            // 欠損検知時の再購読は sequence_gaps と 1:1 なので別途数えていない。
            resyncs: 0,
            // クロスした板は edgeX では異常データとして弾いている
            crossed_books: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_contract_id_from_channel() {
        assert_eq!(
            contract_id_from_channel("depth.10000001.15").as_deref(),
            Some("10000001")
        );
        assert_eq!(contract_id_from_channel("ticker.10000001"), None);
        assert_eq!(contract_id_from_channel("depth"), None);
    }
}
