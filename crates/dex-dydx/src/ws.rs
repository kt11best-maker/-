use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use config::DydxConfig;
use core_types::{Dex, MessageTrace, OrderBook, Symbol};
use dex_traits::{
    Backoff, ConnectionState, ConnectionStatus, MarketDataError, MarketDataSource, SourceCounters,
    SourceMetrics,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

use crate::book_builder::DydxBookBuilder;
use crate::message::{
    subscribe_message, unsubscribe_message, DydxEnvelope, DydxMessageKind, ORDERBOOK_CHANNEL,
};

/// ローカル板が保持するレベル数の上限。
const MAX_TRACKED_LEVELS: usize = 1_000;

/// メッセージ処理の結果、接続へ送るべき操作。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    /// 欠損検知。板を作り直すため購読し直す。
    Resubscribe(String),
}

/// dYdX v4 の板ストリーム。
///
/// # 他の DEX と違う点
///
/// 1. **`connected` を受信してから購読する**（接続直後に送らない）
/// 2. **クロスした板が正常に起こる。** 捨てずにフラグを立てて記録する
/// 3. Ping は WS プロトコルレベルの制御フレームで届く（JSON ではない）
/// 4. Indexer 経由なので板が構造的に「少し古い」可能性がある。
///    `staleness_delta_ms` の分布を必ず確認すること
pub struct DydxMarketData {
    cfg: DydxConfig,
    state: Arc<ConnectionState>,
    counters: Arc<SourceCounters>,
}

impl DydxMarketData {
    pub fn new(cfg: DydxConfig) -> Self {
        DydxMarketData {
            cfg,
            state: Arc::new(ConnectionState::new()),
            counters: Arc::new(SourceCounters::new()),
        }
    }

    pub fn counters(&self) -> &SourceCounters {
        &self.counters
    }

    /// 板を下流に流す。
    ///
    /// **クロスした板も流す。** dYdX ではクロスが構造上正常に起こるため、
    /// ここで捨てるとクロス発生頻度の計測（フェーズ1 の目的の 1 つ）が
    /// できなくなる。判定側での除外は戦略レイヤーの責務。
    async fn emit(
        &self,
        builder: &DydxBookBuilder,
        depth: usize,
        trace: MessageTrace,
        tx: &mpsc::Sender<OrderBook>,
    ) -> Result<(), MarketDataError> {
        let book = builder.to_order_book(depth, trace);

        // ソート順・数量の異常だけを弾く（クロスは許容する）
        if let Err(e) = book.validate_allowing_crossed() {
            self.counters.record_parse_error();
            warn!(dex = %Dex::Dydx, symbol = %book.symbol, error = %e, "板の整合性チェックに失敗");
            return Ok(());
        }
        if book.is_crossed() {
            self.counters.record_crossed_book();
            debug!(
                dex = %Dex::Dydx,
                symbol = %book.symbol,
                best_bid = ?book.best_bid(),
                best_ask = ?book.best_ask(),
                "板がクロスしています（dYdX では正常に起こる。裁定判定からは除外されます）"
            );
        }

        if tx.capacity() == 0 {
            warn!(dex = %Dex::Dydx, "板チャネルが飽和（下流の処理が追いついていない）");
        }
        tx.send(book)
            .await
            .map_err(|_| MarketDataError::ChannelClosed)?;
        self.counters.record_book();
        Ok(())
    }

    /// 1 回の接続セッション。
    async fn session(
        &self,
        symbols: &[Symbol],
        depth: usize,
        tx: &mpsc::Sender<OrderBook>,
        backoff: &mut Backoff,
    ) -> Result<(), MarketDataError> {
        let url = self.cfg.effective_ws_url().to_string();
        let (ws, _resp) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .map_err(|e| MarketDataError::WebSocket(e.to_string()))?;

        self.state.set_connected();
        backoff.reset();
        info!(dex = %Dex::Dydx, url = %url, testnet = self.cfg.use_testnet, "WS 接続確立");

        let (mut write, mut read) = ws.split();

        // 銘柄 → market id（"BTC-USD"）と、その逆引き。
        let markets: HashMap<String, Symbol> = symbols
            .iter()
            .map(|s| (s.to_dex_symbol(Dex::Dydx).to_string(), *s))
            .collect();
        let mut builders: HashMap<String, DydxBookBuilder> = markets
            .iter()
            .map(|(market, symbol)| {
                (
                    market.clone(),
                    DydxBookBuilder::new(*symbol, depth.max(MAX_TRACKED_LEVELS)),
                )
            })
            .collect();

        // 手順2-3: connected を待ってから購読する。接続直後に送ってはいけない。
        let mut subscribed = false;
        let connected_deadline =
            tokio::time::Instant::now() + Duration::from_secs(self.cfg.connected_timeout_secs);

        loop {
            tokio::select! {
                incoming = read.next() => {
                    let Some(msg) = incoming else {
                        warn!(dex = %Dex::Dydx, "WS ストリームが終了");
                        return Ok(());
                    };
                    let msg = msg.map_err(|e| MarketDataError::WebSocket(e.to_string()))?;
                    match msg {
                        Message::Text(text) => {
                            // パース前に受信時刻を確定させる（レイテンシ計測の起点）
                            let trace = MessageTrace::on_receive();
                            self.counters.record_message();

                            let (actions, connected) = self
                                .handle_text(text.as_str(), depth, trace, &markets, &mut builders, tx)
                                .await?;

                            if connected && !subscribed {
                                for market in markets.keys() {
                                    write
                                        .send(Message::text(subscribe_message(market)))
                                        .await
                                        .map_err(|e| MarketDataError::Subscribe(e.to_string()))?;
                                    debug!(dex = %Dex::Dydx, market = %market, channel = ORDERBOOK_CHANNEL, "板を購読");
                                }
                                subscribed = true;
                            }

                            for action in actions {
                                let Action::Resubscribe(market) = action;
                                write
                                    .send(Message::text(unsubscribe_message(&market)))
                                    .await
                                    .map_err(|e| MarketDataError::WebSocket(e.to_string()))?;
                                write
                                    .send(Message::text(subscribe_message(&market)))
                                    .await
                                    .map_err(|e| MarketDataError::WebSocket(e.to_string()))?;
                            }
                        }
                        // dYdX は 30 秒ごとにプロトコルレベルの ping を送ってくる。
                        // 10 秒以内に pong を返さないと切断されるため、
                        // ライブラリの自動応答に頼らず明示的に返す。
                        Message::Ping(payload) => {
                            write
                                .send(Message::Pong(payload))
                                .await
                                .map_err(|e| MarketDataError::WebSocket(e.to_string()))?;
                        }
                        Message::Close(frame) => {
                            warn!(dex = %Dex::Dydx, ?frame, "WS クローズを受信");
                            return Ok(());
                        }
                        Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {}
                    }
                }
                _ = tokio::time::sleep_until(connected_deadline), if !subscribed => {
                    // connected が来ないまま時間切れ。接続をやり直す。
                    warn!(
                        dex = %Dex::Dydx,
                        timeout_secs = self.cfg.connected_timeout_secs,
                        "connected メッセージが届きません。接続をやり直します"
                    );
                    return Ok(());
                }
            }
        }
    }

    /// 1 メッセージを処理する。返り値は (送るべき操作, `connected` を受信したか)。
    async fn handle_text(
        &self,
        text: &str,
        depth: usize,
        trace: MessageTrace,
        markets: &HashMap<String, Symbol>,
        builders: &mut HashMap<String, DydxBookBuilder>,
        tx: &mpsc::Sender<OrderBook>,
    ) -> Result<(Vec<Action>, bool), MarketDataError> {
        let env: DydxEnvelope = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                self.counters.record_parse_error();
                warn!(
                    dex = %Dex::Dydx,
                    error = %e,
                    snippet = %text.chars().take(200).collect::<String>(),
                    "メッセージのパースに失敗"
                );
                return Ok((Vec::new(), false));
            }
        };

        match env.message_kind() {
            DydxMessageKind::Connected => {
                info!(dex = %Dex::Dydx, connection_id = ?env.connection_id, "connected を受信。購読を開始します");
                Ok((Vec::new(), true))
            }
            DydxMessageKind::Error => {
                error!(
                    dex = %Dex::Dydx,
                    message = ?env.message,
                    snippet = %text.chars().take(300).collect::<String>(),
                    "取引所からエラー応答"
                );
                Ok((Vec::new(), false))
            }
            DydxMessageKind::Unsubscribed => {
                debug!(dex = %Dex::Dydx, market = ?env.id, "購読解除の応答");
                Ok((Vec::new(), false))
            }
            DydxMessageKind::Subscribed | DydxMessageKind::ChannelData => {
                let actions = self
                    .handle_orderbook(&env, depth, trace, markets, builders, tx)
                    .await?;
                Ok((actions, false))
            }
            DydxMessageKind::Unknown => {
                debug!(
                    dex = %Dex::Dydx,
                    snippet = %text.chars().take(200).collect::<String>(),
                    "未知のメッセージ形式"
                );
                Ok((Vec::new(), false))
            }
        }
    }

    async fn handle_orderbook(
        &self,
        env: &DydxEnvelope,
        depth: usize,
        trace: MessageTrace,
        markets: &HashMap<String, Symbol>,
        builders: &mut HashMap<String, DydxBookBuilder>,
        tx: &mpsc::Sender<OrderBook>,
    ) -> Result<Vec<Action>, MarketDataError> {
        if !env.is_orderbook() {
            debug!(dex = %Dex::Dydx, channel = ?env.channel, "板以外のチャンネルを無視");
            return Ok(Vec::new());
        }
        let Some(market) = env.id.as_deref() else {
            debug!(dex = %Dex::Dydx, "id の無い板メッセージを無視");
            return Ok(Vec::new());
        };
        if !markets.contains_key(market) {
            // 購読していない市場。dYdX に存在しない銘柄も普通にありうる
            debug!(dex = %Dex::Dydx, market = %market, "未購読の市場を無視");
            return Ok(Vec::new());
        }
        let Some(contents) = env.order_book() else {
            debug!(dex = %Dex::Dydx, market = %market, "板の内容が読めないメッセージを無視");
            return Ok(Vec::new());
        };
        let Some(builder) = builders.get_mut(market) else {
            return Ok(Vec::new());
        };

        match env.message_kind() {
            DydxMessageKind::Subscribed => {
                // 全量スナップショット。ローカル板を必ずリセットしてから作り直す。
                builder.apply_snapshot(&contents, env.message_id());
                self.counters.record_resync();
                debug!(
                    dex = %Dex::Dydx,
                    market = %market,
                    message_id = ?env.message_id(),
                    "板をスナップショットから初期化"
                );
            }
            _ => {
                if let Err(e) = builder.apply_update(&contents, env.message_id()) {
                    self.counters.record_sequence_gap();
                    warn!(
                        dex = %Dex::Dydx,
                        market = %market,
                        error = %e,
                        "差分の適用に失敗。板を作り直します"
                    );
                    builder.reset();
                    return Ok(vec![Action::Resubscribe(market.to_string())]);
                }
            }
        }

        let builder = &builders[market];
        self.emit(builder, depth, trace, tx).await?;
        Ok(Vec::new())
    }
}

#[async_trait]
impl MarketDataSource for DydxMarketData {
    fn dex(&self) -> Dex {
        Dex::Dydx
    }

    async fn subscribe_orderbooks(
        &self,
        symbols: &[Symbol],
        depth: usize,
        tx: mpsc::Sender<OrderBook>,
    ) -> Result<(), MarketDataError> {
        if symbols.is_empty() {
            return Err(MarketDataError::Config(
                "購読する銘柄が指定されていません".to_string(),
            ));
        }

        let policy = self.cfg.reconnect_policy();
        let mut backoff = Backoff::new(
            policy.base_delay_ms,
            policy.max_delay_ms,
            policy.max_attempts,
        );

        loop {
            match self.session(symbols, depth, &tx, &mut backoff).await {
                Ok(()) => {
                    warn!(dex = %Dex::Dydx, "セッションが切断されました");
                }
                Err(MarketDataError::ChannelClosed) => {
                    self.state.set_disconnected();
                    info!(dex = %Dex::Dydx, "下流チャネルが閉じたため購読を終了");
                    return Err(MarketDataError::ChannelClosed);
                }
                Err(e) => {
                    warn!(dex = %Dex::Dydx, error = %e, "セッションが異常終了");
                }
            }

            match backoff.next_delay() {
                Some(delay) => {
                    self.state.set_reconnecting(backoff.attempt());
                    info!(
                        dex = %Dex::Dydx,
                        attempt = backoff.attempt(),
                        delay_ms = delay.as_millis() as u64,
                        "再接続を待機"
                    );
                    tokio::time::sleep(delay).await;
                }
                None => {
                    self.state.set_disconnected();
                    error!(dex = %Dex::Dydx, attempts = backoff.attempt(), "再接続の上限に到達");
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
        self.counters.snapshot()
    }
}
