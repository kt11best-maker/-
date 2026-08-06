use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use config::HyperliquidConfig;
use core_types::{Dex, FundingRate, MessageTrace, OrderBook, Symbol};
use dex_traits::{
    Backoff, ConnectionState, ConnectionStatus, FundingChannel, FundingRateSource, MarketDataError,
    MarketDataSource, SourceMetrics,
};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

use crate::message::{ping_message, to_funding_rate, to_order_book, HlMessage, SubscribeRequest};

/// Hyperliquid の板ストリーム。
///
/// `subscribe_orderbooks` は内部で再接続を繰り返し、上限に到達したときだけ
/// エラーを返して終了する。
pub struct HyperliquidMarketData {
    cfg: HyperliquidConfig,
    state: Arc<ConnectionState>,
    messages_received: Arc<AtomicU64>,
    books_emitted: Arc<AtomicU64>,
    parse_errors: Arc<AtomicU64>,
    /// ファンディングレートの送信先。`activeAssetCtx` は板と**同じ接続**に
    /// 相乗りさせるため、接続は増えない。
    funding: Arc<FundingChannel>,
    /// `activeAssetCtx` を購読するか。
    collect_funding: bool,
    /// API が精算間隔を返さないためのフォールバック定数（時間）。
    funding_interval_hours: Option<Decimal>,
}

impl HyperliquidMarketData {
    pub fn new(cfg: HyperliquidConfig) -> Self {
        HyperliquidMarketData {
            cfg,
            state: Arc::new(ConnectionState::new()),
            messages_received: Arc::new(AtomicU64::new(0)),
            books_emitted: Arc::new(AtomicU64::new(0)),
            parse_errors: Arc::new(AtomicU64::new(0)),
            funding: Arc::new(FundingChannel::new()),
            collect_funding: false,
            funding_interval_hours: None,
        }
    }

    /// ファンディング収集の有効化と、精算間隔のフォールバック定数
    /// （`[funding.intervals_hours]`）を設定する。
    ///
    /// 無効なら `activeAssetCtx` を購読しないので、板だけを取る従来どおりの
    /// トラフィックになる。
    pub fn with_funding(mut self, enabled: bool, interval_hours: Option<Decimal>) -> Self {
        self.collect_funding = enabled;
        self.funding_interval_hours = interval_hours;
        self
    }

    pub fn funding_channel(&self) -> &FundingChannel {
        &self.funding
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

    /// 1 回の接続セッション。切断されたら `Ok(())` で戻り、呼び出し元が再接続する。
    async fn session(
        &self,
        symbols: &[Symbol],
        depth: usize,
        tx: &mpsc::Sender<OrderBook>,
        backoff: &mut Backoff,
    ) -> Result<(), MarketDataError> {
        let (ws, _resp) = tokio_tungstenite::connect_async(self.cfg.ws_url.as_str())
            .await
            .map_err(|e| MarketDataError::WebSocket(e.to_string()))?;

        self.state.set_connected();
        backoff.reset();
        info!(dex = %Dex::Hyperliquid, url = %self.cfg.ws_url, "WS 接続確立");

        let (mut write, mut read) = ws.split();

        for symbol in symbols {
            let req = serde_json::to_string(&SubscribeRequest::l2_book(*symbol))
                .map_err(|e| MarketDataError::Subscribe(e.to_string()))?;
            write
                .send(Message::text(req))
                .await
                .map_err(|e| MarketDataError::Subscribe(e.to_string()))?;
            debug!(dex = %Dex::Hyperliquid, symbol = %symbol, "l2Book を購読");

            // ファンディングは同じ接続に相乗りさせる（接続を増やさない）。
            // 失敗しても板の収集は続けたいので、ここでは購読エラーを致命にしない。
            if self.collect_funding {
                let req = serde_json::to_string(&SubscribeRequest::active_asset_ctx(*symbol))
                    .map_err(|e| MarketDataError::Subscribe(e.to_string()))?;
                match write.send(Message::text(req)).await {
                    Ok(()) => {
                        debug!(dex = %Dex::Hyperliquid, symbol = %symbol, "activeAssetCtx を購読")
                    }
                    Err(e) => warn!(
                        dex = %Dex::Hyperliquid,
                        symbol = %symbol,
                        error = %e,
                        "activeAssetCtx の購読に失敗（板の収集は継続します）"
                    ),
                }
            }
        }

        let mut ping_ticker =
            tokio::time::interval(Duration::from_secs(self.cfg.ping_interval_secs.max(1)));
        ping_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // 起動直後の 1 回目の tick は即座に発火するので捨てる
        ping_ticker.tick().await;

        loop {
            tokio::select! {
                incoming = read.next() => {
                    let Some(msg) = incoming else {
                        warn!(dex = %Dex::Hyperliquid, "WS ストリームが終了");
                        return Ok(());
                    };
                    let msg = msg.map_err(|e| MarketDataError::WebSocket(e.to_string()))?;
                    match msg {
                        Message::Text(text) => {
                            // パース前に受信時刻を確定させる（レイテンシ計測の起点）
                            let trace = MessageTrace::on_receive();
                            self.messages_received.fetch_add(1, Ordering::Relaxed);
                            self.handle_text(text.as_str(), depth, trace, tx).await?;
                        }
                        Message::Ping(payload) => {
                            write
                                .send(Message::Pong(payload))
                                .await
                                .map_err(|e| MarketDataError::WebSocket(e.to_string()))?;
                        }
                        Message::Close(frame) => {
                            warn!(dex = %Dex::Hyperliquid, ?frame, "WS クローズを受信");
                            return Ok(());
                        }
                        Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {}
                    }
                }
                _ = ping_ticker.tick() => {
                    if let Err(e) = write.send(Message::text(ping_message())).await {
                        warn!(dex = %Dex::Hyperliquid, error = %e, "ping 送信に失敗");
                        return Err(MarketDataError::WebSocket(e.to_string()));
                    }
                }
            }
        }
    }

    async fn handle_text(
        &self,
        text: &str,
        depth: usize,
        trace: MessageTrace,
        tx: &mpsc::Sender<OrderBook>,
    ) -> Result<(), MarketDataError> {
        let parsed: HlMessage = match serde_json::from_str(text) {
            Ok(m) => m,
            Err(e) => {
                self.parse_errors.fetch_add(1, Ordering::Relaxed);
                warn!(
                    dex = %Dex::Hyperliquid,
                    error = %e,
                    // 想定外の形式を後から追えるよう先頭のみ残す（全文はログ肥大の原因）
                    snippet = %text.chars().take(200).collect::<String>(),
                    "メッセージのパースに失敗"
                );
                return Ok(());
            }
        };

        match parsed {
            HlMessage::L2Book { data } => match to_order_book(&data, depth, trace) {
                Ok(book) => {
                    if tx.capacity() == 0 {
                        warn!(dex = %Dex::Hyperliquid, "板チャネルが飽和（下流の処理が追いついていない）");
                    }
                    tx.send(book)
                        .await
                        .map_err(|_| MarketDataError::ChannelClosed)?;
                    self.books_emitted.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    self.parse_errors.fetch_add(1, Ordering::Relaxed);
                    debug!(dex = %Dex::Hyperliquid, error = %e, "板の正規化をスキップ");
                }
            },
            HlMessage::ActiveAssetCtx { data } => {
                // ファンディングのパース失敗は板の処理に影響させない
                match to_funding_rate(&data, self.funding_interval_hours, trace) {
                    Some(rate) => {
                        self.funding.publish(rate);
                    }
                    None => debug!(
                        dex = %Dex::Hyperliquid,
                        coin = %data.coin,
                        "ファンディングを含まない activeAssetCtx をスキップ"
                    ),
                }
            }
            HlMessage::SubscriptionResponse { data } => {
                debug!(dex = %Dex::Hyperliquid, response = %data, "購読応答");
            }
            HlMessage::Error { data } => {
                error!(dex = %Dex::Hyperliquid, response = %data, "取引所からエラー応答");
            }
            HlMessage::Pong | HlMessage::Other => {}
        }
        Ok(())
    }
}

#[async_trait]
impl MarketDataSource for HyperliquidMarketData {
    fn dex(&self) -> Dex {
        Dex::Hyperliquid
    }

    async fn subscribe_orderbooks(
        &self,
        symbols: &[Symbol],
        depth: usize,
        tx: mpsc::Sender<OrderBook>,
    ) -> Result<(), MarketDataError> {
        let policy = self.cfg.reconnect_policy();
        let mut backoff = Backoff::new(
            policy.base_delay_ms,
            policy.max_delay_ms,
            policy.max_attempts,
        );

        loop {
            match self.session(symbols, depth, &tx, &mut backoff).await {
                Ok(()) => {
                    warn!(dex = %Dex::Hyperliquid, "セッションが切断されました");
                }
                Err(MarketDataError::ChannelClosed) => {
                    self.state.set_disconnected();
                    info!(dex = %Dex::Hyperliquid, "下流チャネルが閉じたため購読を終了");
                    return Err(MarketDataError::ChannelClosed);
                }
                Err(e) => {
                    warn!(dex = %Dex::Hyperliquid, error = %e, "セッションが異常終了");
                }
            }

            match backoff.next_delay() {
                Some(delay) => {
                    self.state.set_reconnecting(backoff.attempt());
                    info!(
                        dex = %Dex::Hyperliquid,
                        attempt = backoff.attempt(),
                        delay_ms = delay.as_millis() as u64,
                        "再接続を待機"
                    );
                    tokio::time::sleep(delay).await;
                }
                None => {
                    self.state.set_disconnected();
                    error!(dex = %Dex::Hyperliquid, attempts = backoff.attempt(), "再接続の上限に到達");
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
            // l2Book は毎回フルスナップショットなので、シーケンス欠損も
            // 板の作り直しも概念として存在しない。
            sequence_gaps: 0,
            resyncs: 0,
            // クロスした板は Hyperliquid では異常データとして弾いている
            crossed_books: 0,
        }
    }
}

/// ファンディングは `activeAssetCtx` を板と同じ接続で購読して取る。
///
/// ここでやるのは送信先の登録だけで、実際のレートは板と同じセッションの
/// 受信ループから流れる。板側が再接続すれば購読もやり直されるため、
/// ファンディング側で再接続処理を持つ必要はない。
#[async_trait]
impl FundingRateSource for HyperliquidMarketData {
    fn dex(&self) -> Dex {
        Dex::Hyperliquid
    }

    async fn subscribe_funding(
        &self,
        _symbols: &[Symbol],
        tx: mpsc::Sender<FundingRate>,
    ) -> Result<(), MarketDataError> {
        if !self.collect_funding {
            return Err(MarketDataError::Config(
                "activeAssetCtx の購読が無効です（with_funding を確認してください）".to_string(),
            ));
        }
        self.funding.serve(Dex::Hyperliquid, tx).await
    }
}
