use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use config::LighterConfig;
use core_types::{Dex, FundingRate, MessageTrace, OrderBook, Price, Symbol};
use dex_traits::{
    Backoff, ConnectionState, ConnectionStatus, FundingChannel, FundingRateSource, MarketDataError,
    MarketDataSource, SourceCounters, SourceMetrics,
};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

use crate::book_builder::LighterBookBuilder;
use crate::message::{
    collect_market_stats, order_book_channel, ping_message, pong_message, subscribe_message,
    unsubscribe_message, LighterEnvelope, LighterMessageKind, MarketStatsEntry, UpdateKind,
    MARKET_STATS_CHANNEL,
};

/// ローカル板が保持するレベル数の上限。
const MAX_TRACKED_LEVELS: usize = 1_000;

/// メッセージ処理の結果、接続へ送るべき操作。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    /// アプリ層 ping への応答。
    Pong,
    /// 新たに解決した market_index の板を購読する。
    Subscribe(String),
    /// 欠損検知。板を作り直すため購読し直す。
    Resubscribe(String),
}

/// Lighter の板ストリーム。
///
/// **フェーズ1 では REST を一切使わない。** 板もシンボルマッピングも WS で
/// 完結するため、REST 側のレートリミットや IP ban を考慮する必要がない。
pub struct LighterMarketData {
    cfg: LighterConfig,
    state: Arc<ConnectionState>,
    counters: Arc<SourceCounters>,
    /// ファンディングレートの送信先。板と**同じ接続**の `market_stats` から
    /// 分岐させるので、ここは送信先スロットを持つだけで接続は増えない。
    funding: Arc<FundingChannel>,
    /// `market_stats` からファンディングを取り出すか。
    collect_funding: bool,
    /// API が精算間隔を返さないためのフォールバック定数（時間）。
    /// 設定されていなければ `interval_hours = None` として記録する。
    funding_interval_hours: Option<Decimal>,
}

impl LighterMarketData {
    pub fn new(cfg: LighterConfig) -> Self {
        LighterMarketData {
            cfg,
            state: Arc::new(ConnectionState::new()),
            counters: Arc::new(SourceCounters::new()),
            funding: Arc::new(FundingChannel::new()),
            collect_funding: false,
            funding_interval_hours: None,
        }
    }

    /// ファンディング収集の有効化と、精算間隔のフォールバック定数
    /// （`[funding.intervals_hours]`）を設定する。
    ///
    /// Lighter はもともと `market_stats:all` を購読しているので、有効にしても
    /// **購読も接続も増えない**（受信済みメッセージから分岐するだけ）。
    pub fn with_funding(mut self, enabled: bool, interval_hours: Option<Decimal>) -> Self {
        self.collect_funding = enabled;
        self.funding_interval_hours = interval_hours;
        self
    }

    pub fn counters(&self) -> &SourceCounters {
        &self.counters
    }

    pub fn funding_channel(&self) -> &FundingChannel {
        &self.funding
    }

    async fn emit(
        &self,
        builder: &LighterBookBuilder,
        depth: usize,
        trace: MessageTrace,
        tx: &mpsc::Sender<OrderBook>,
    ) -> Result<(), MarketDataError> {
        let book = builder.to_order_book(depth, trace);
        if let Err(e) = book.validate() {
            self.counters.record_parse_error();
            warn!(dex = %Dex::Lighter, symbol = %book.symbol, error = %e, "板の整合性チェックに失敗");
            return Ok(());
        }
        if tx.capacity() == 0 {
            warn!(dex = %Dex::Lighter, "板チャネルが飽和（下流の処理が追いついていない）");
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
        info!(dex = %Dex::Lighter, url = %url, testnet = self.cfg.use_testnet, "WS 接続確立");

        let (mut write, mut read) = ws.split();

        // 手順1: market_stats:all を購読して symbol → market_index を動的に構築する。
        write
            .send(Message::text(subscribe_message(MARKET_STATS_CHANNEL)))
            .await
            .map_err(|e| MarketDataError::Subscribe(e.to_string()))?;
        debug!(dex = %Dex::Lighter, channel = MARKET_STATS_CHANNEL, "シンボルマッピングを購読");

        let targets: HashSet<Symbol> = symbols.iter().copied().collect();
        let mut mapping: HashMap<Symbol, u32> = HashMap::new();
        let mut builders: HashMap<u32, LighterBookBuilder> = HashMap::new();

        // Lighter は 2 分間フレームを送らないと切断する（こちらから送る責任がある）。
        let mut keepalive = tokio::time::interval(Duration::from_secs(
            self.cfg.keepalive_interval_secs.clamp(1, 110),
        ));
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        keepalive.tick().await;

        let mapping_deadline = Instant::now() + Duration::from_secs(self.cfg.mapping_warn_secs);
        let mut mapping_warned = false;

        loop {
            tokio::select! {
                incoming = read.next() => {
                    let Some(msg) = incoming else {
                        warn!(dex = %Dex::Lighter, "WS ストリームが終了");
                        return Ok(());
                    };
                    let msg = msg.map_err(|e| MarketDataError::WebSocket(e.to_string()))?;
                    match msg {
                        Message::Text(text) => {
                            // パース前に受信時刻を確定させる（レイテンシ計測の起点）
                            let trace = MessageTrace::on_receive();
                            self.counters.record_message();
                            let actions = self
                                .handle_text(
                                    text.as_str(),
                                    depth,
                                    trace,
                                    &targets,
                                    &mut mapping,
                                    &mut builders,
                                    tx,
                                )
                                .await?;
                            for action in actions {
                                match action {
                                    Action::Pong => {
                                        write
                                            .send(Message::text(pong_message()))
                                            .await
                                            .map_err(|e| MarketDataError::WebSocket(e.to_string()))?;
                                    }
                                    Action::Subscribe(channel) => {
                                        write
                                            .send(Message::text(subscribe_message(&channel)))
                                            .await
                                            .map_err(|e| MarketDataError::Subscribe(e.to_string()))?;
                                    }
                                    Action::Resubscribe(channel) => {
                                        write
                                            .send(Message::text(unsubscribe_message(&channel)))
                                            .await
                                            .map_err(|e| MarketDataError::WebSocket(e.to_string()))?;
                                        write
                                            .send(Message::text(subscribe_message(&channel)))
                                            .await
                                            .map_err(|e| MarketDataError::WebSocket(e.to_string()))?;
                                    }
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
                            warn!(dex = %Dex::Lighter, ?frame, "WS クローズを受信");
                            return Ok(());
                        }
                        Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {}
                    }
                }
                _ = keepalive.tick() => {
                    if let Err(e) = write.send(Message::text(ping_message())).await {
                        warn!(dex = %Dex::Lighter, error = %e, "keepalive の送信に失敗");
                        return Err(MarketDataError::WebSocket(e.to_string()));
                    }
                }
                _ = tokio::time::sleep_until(mapping_deadline.into()), if !mapping_warned => {
                    mapping_warned = true;
                    let unresolved: Vec<String> = targets
                        .iter()
                        .filter(|s| !mapping.contains_key(s))
                        .map(|s| s.to_string())
                        .collect();
                    if !unresolved.is_empty() {
                        // 市場が存在しない銘柄はここに出る。異常ではないが、
                        // 収集対象から落ちていることは把握できるようにする。
                        warn!(
                            dex = %Dex::Lighter,
                            symbols = ?unresolved,
                            elapsed_secs = self.cfg.mapping_warn_secs,
                            "market_stats から market_index を解決できていない銘柄があります"
                        );
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_text(
        &self,
        text: &str,
        depth: usize,
        trace: MessageTrace,
        targets: &HashSet<Symbol>,
        mapping: &mut HashMap<Symbol, u32>,
        builders: &mut HashMap<u32, LighterBookBuilder>,
        tx: &mpsc::Sender<OrderBook>,
    ) -> Result<Vec<Action>, MarketDataError> {
        let env: LighterEnvelope = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                self.counters.record_parse_error();
                warn!(
                    dex = %Dex::Lighter,
                    error = %e,
                    snippet = %text.chars().take(200).collect::<String>(),
                    "メッセージのパースに失敗"
                );
                return Ok(Vec::new());
            }
        };

        match env.message_kind() {
            LighterMessageKind::Ping => Ok(vec![Action::Pong]),
            LighterMessageKind::Pong | LighterMessageKind::Control => Ok(Vec::new()),
            LighterMessageKind::Error => {
                error!(
                    dex = %Dex::Lighter,
                    message = %text.chars().take(300).collect::<String>(),
                    "取引所からエラー応答"
                );
                Ok(Vec::new())
            }
            LighterMessageKind::MarketStats => {
                Ok(self.handle_market_stats(&env, trace, targets, mapping, builders, depth))
            }
            LighterMessageKind::OrderBook => {
                self.handle_order_book(&env, depth, trace, builders, tx)
                    .await
            }
            LighterMessageKind::Unknown => {
                debug!(
                    dex = %Dex::Lighter,
                    snippet = %text.chars().take(200).collect::<String>(),
                    "未知のメッセージ形式"
                );
                Ok(Vec::new())
            }
        }
    }

    /// `market_stats` から symbol → market_index を更新し、新規に解決できた
    /// 銘柄の板を購読する。**ファンディングレートもここで分岐させる。**
    ///
    /// `market_stats` は継続的に流れてくるため、マッピングの変化にも自動追随する。
    #[allow(clippy::too_many_arguments)]
    fn handle_market_stats(
        &self,
        env: &LighterEnvelope,
        trace: MessageTrace,
        targets: &HashSet<Symbol>,
        mapping: &mut HashMap<Symbol, u32>,
        builders: &mut HashMap<u32, LighterBookBuilder>,
        depth: usize,
    ) -> Vec<Action> {
        let Some(stats) = env.market_stats.as_ref() else {
            return Vec::new();
        };
        let exchange_ts_ms = env.exchange_ts_ms();

        let mut actions = Vec::new();
        for entry in collect_market_stats(stats) {
            let market_index = entry.market_id;
            let Some(symbol) = Symbol::from_dex_symbol(Dex::Lighter, &entry.symbol) else {
                continue;
            };
            if !targets.contains(&symbol) {
                continue;
            }

            // ファンディングはマッピングの更新有無と無関係に毎回流す
            // （market_index が変わるのは初回だけなので、ここを先に処理する）。
            self.publish_funding(&entry, symbol, exchange_ts_ms, trace);

            match mapping.get(&symbol) {
                Some(existing) if *existing == market_index => continue,
                Some(existing) => {
                    // market_index が変わることは想定しにくいが、変化にも追随する
                    warn!(
                        dex = %Dex::Lighter,
                        symbol = %symbol,
                        old = existing,
                        new = market_index,
                        "market_index が変更されました。購読し直します"
                    );
                    builders.remove(existing);
                }
                None => {}
            }

            mapping.insert(symbol, market_index);
            builders.insert(
                market_index,
                LighterBookBuilder::new(symbol, market_index, depth.max(MAX_TRACKED_LEVELS)),
            );
            let channel = order_book_channel(market_index);
            info!(
                dex = %Dex::Lighter,
                symbol = %symbol,
                market_index,
                "market_index を解決。板を購読します"
            );
            actions.push(Action::Subscribe(channel));
        }
        actions
    }

    /// `market_stats` の 1 市場分からファンディングレートを組み立てて流す。
    ///
    /// 送信は必ずノンブロッキング（[`FundingChannel::publish`]）。ここで待つと
    /// 板の受信ループが止まるため、詰まっていれば捨てる。
    fn publish_funding(
        &self,
        entry: &MarketStatsEntry,
        symbol: Symbol,
        exchange_ts_ms: Option<u64>,
        mut trace: MessageTrace,
    ) {
        if !self.collect_funding || !self.funding.is_registered() {
            return;
        }
        // レートが 1 つも入っていないメッセージは記録しても意味がない
        let Some(current_rate) = entry.current_funding_rate.or(entry.funding_rate) else {
            return;
        };

        trace.set_exchange_ts_ms(exchange_ts_ms);
        trace.mark_normalized();

        self.funding.publish(FundingRate {
            dex: Dex::Lighter,
            symbol,
            current_rate,
            // `current_funding_rate` が無いときは `funding_rate` を現在値に昇格
            // させているので、その場合は予測値として重複させない。
            predicted_rate: entry
                .current_funding_rate
                .is_some()
                .then_some(entry.funding_rate)
                .flatten(),
            interval_hours: self.funding_interval_hours,
            next_funding_time_ms: entry.next_funding_time_ms,
            index_price: entry.index_price.map(Price),
            mark_price: entry.mark_price.map(Price),
            trace,
        });
    }

    async fn handle_order_book(
        &self,
        env: &LighterEnvelope,
        depth: usize,
        trace: MessageTrace,
        builders: &mut HashMap<u32, LighterBookBuilder>,
        tx: &mpsc::Sender<OrderBook>,
    ) -> Result<Vec<Action>, MarketDataError> {
        let Some(market_index) = env.market_index() else {
            debug!(dex = %Dex::Lighter, channel = ?env.channel, "market_index を特定できないメッセージを無視");
            return Ok(Vec::new());
        };
        let Some(payload) = env.order_book.as_ref() else {
            return Ok(Vec::new());
        };
        let Some(builder) = builders.get_mut(&market_index) else {
            debug!(dex = %Dex::Lighter, market_index, "未購読の market_index を無視");
            return Ok(Vec::new());
        };

        // type が無い形式でも動くよう、ローカル板の状態でスナップショット/差分を決める。
        let kind = env.update_kind().unwrap_or(if builder.is_initialized() {
            UpdateKind::Update
        } else {
            UpdateKind::Snapshot
        });

        match kind {
            UpdateKind::Snapshot => {
                builder.apply_snapshot(payload, env.nonce(), env.exchange_ts_ms());
                self.counters.record_resync();
                debug!(
                    dex = %Dex::Lighter,
                    symbol = %builder.symbol(),
                    nonce = ?env.nonce(),
                    "板をスナップショットから初期化"
                );
            }
            UpdateKind::Update => {
                if let Err(e) = builder.apply_update(
                    payload,
                    env.begin_nonce(),
                    env.nonce(),
                    env.exchange_ts_ms(),
                ) {
                    self.counters.record_sequence_gap();
                    warn!(
                        dex = %Dex::Lighter,
                        symbol = %builder.symbol(),
                        error = %e,
                        "差分の適用に失敗。板を作り直します"
                    );
                    builder.reset();
                    return Ok(vec![Action::Resubscribe(order_book_channel(market_index))]);
                }
            }
        }

        let builder = &builders[&market_index];
        self.emit(builder, depth, trace, tx).await?;
        Ok(Vec::new())
    }
}

#[async_trait]
impl MarketDataSource for LighterMarketData {
    fn dex(&self) -> Dex {
        Dex::Lighter
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
                    warn!(dex = %Dex::Lighter, "セッションが切断されました");
                }
                Err(MarketDataError::ChannelClosed) => {
                    self.state.set_disconnected();
                    info!(dex = %Dex::Lighter, "下流チャネルが閉じたため購読を終了");
                    return Err(MarketDataError::ChannelClosed);
                }
                Err(e) => {
                    warn!(dex = %Dex::Lighter, error = %e, "セッションが異常終了");
                }
            }

            match backoff.next_delay() {
                Some(delay) => {
                    self.state.set_reconnecting(backoff.attempt());
                    info!(
                        dex = %Dex::Lighter,
                        attempt = backoff.attempt(),
                        delay_ms = delay.as_millis() as u64,
                        "再接続を待機"
                    );
                    tokio::time::sleep(delay).await;
                }
                None => {
                    self.state.set_disconnected();
                    error!(dex = %Dex::Lighter, attempts = backoff.attempt(), "再接続の上限に到達");
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

/// ファンディングは既存の `market_stats:all` 購読から分岐させる。
///
/// **接続は増えない。** ここでやるのは送信先の登録だけで、実際のレートは
/// 板と同じセッションの受信ループから流れる。板側が再接続しても登録は
/// 維持されるため、ファンディング側で再接続処理を持つ必要もない。
#[async_trait]
impl FundingRateSource for LighterMarketData {
    fn dex(&self) -> Dex {
        Dex::Lighter
    }

    async fn subscribe_funding(
        &self,
        _symbols: &[Symbol],
        tx: mpsc::Sender<FundingRate>,
    ) -> Result<(), MarketDataError> {
        if !self.collect_funding {
            return Err(MarketDataError::Config(
                "ファンディング収集が無効です（with_funding を確認してください）".to_string(),
            ));
        }
        self.funding.serve(Dex::Lighter, tx).await
    }
}
