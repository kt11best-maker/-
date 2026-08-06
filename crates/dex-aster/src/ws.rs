use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use config::AsterConfig;
use core_types::{Dex, MessageTrace, OrderBook, Symbol};
use dex_traits::{
    Backoff, ConnectionState, ConnectionStatus, MarketDataError, MarketDataSource, SourceCounters,
    SourceMetrics,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

use crate::book_builder::{ApplyOutcome, AsterBookBuilder};
use crate::message::{combined_stream_url, parse_depth_message, DepthSnapshot};
use crate::rest::{build_client, fetch_depth_snapshot};

/// ローカル板が保持するレベル数の上限。
const MAX_TRACKED_LEVELS: usize = 5_000;

/// スナップショット結果の受け渡しチャネル容量。
const SNAPSHOT_CHANNEL_CAPACITY: usize = 16;

type SnapshotResult = (Symbol, Result<DepthSnapshot, MarketDataError>);

/// Aster の板ストリーム。
///
/// WS の差分と REST スナップショットを突き合わせて板を再構築する。REST は
/// 初期化時と再同期時にしか叩かず、`resync_backoff_ms` で最小間隔を強制する
/// （レートリミットは IP 単位で、違反を繰り返すと最大 3 日 ban されるため）。
pub struct AsterMarketData {
    cfg: AsterConfig,
    state: Arc<ConnectionState>,
    counters: Arc<SourceCounters>,
    http: reqwest::Client,
}

impl AsterMarketData {
    pub fn new(cfg: AsterConfig) -> Result<Self, MarketDataError> {
        Ok(AsterMarketData {
            cfg,
            state: Arc::new(ConnectionState::new()),
            counters: Arc::new(SourceCounters::new()),
            http: build_client()?,
        })
    }

    pub fn counters(&self) -> &SourceCounters {
        &self.counters
    }

    /// スナップショット取得タスクを投入する。
    ///
    /// `next_allowed` を進めることで、複数銘柄が同時に再同期に落ちても REST が
    /// バーストしないよう**全銘柄まとめて**間隔を空ける（ban は IP 単位なので、
    /// 銘柄ごとのバックオフでは不十分）。
    fn schedule_snapshot(
        &self,
        symbol: Symbol,
        next_allowed: &mut Instant,
        pending: &mut HashSet<Symbol>,
        snap_tx: &mpsc::Sender<SnapshotResult>,
    ) {
        if !pending.insert(symbol) {
            // 取得中。二重に叩かない。
            return;
        }

        let now = Instant::now();
        let fire_at = (*next_allowed).max(now);
        let delay = fire_at.saturating_duration_since(now);
        *next_allowed = fire_at + Duration::from_millis(self.cfg.resync_backoff_ms);

        let url = self.cfg.snapshot_url(symbol);
        let client = self.http.clone();
        let tx = snap_tx.clone();

        debug!(
            dex = %Dex::Aster,
            symbol = %symbol,
            delay_ms = delay.as_millis() as u64,
            "板スナップショットの取得を予約"
        );

        tokio::spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let result = fetch_depth_snapshot(&client, &url).await;
            // セッションが終わっていれば送信先は閉じている。無視してよい。
            let _ = tx.send((symbol, result)).await;
        });
    }

    /// 板を正規化して下流に流す。
    async fn emit(
        &self,
        builder: &AsterBookBuilder,
        depth: usize,
        trace: MessageTrace,
        tx: &mpsc::Sender<OrderBook>,
    ) -> Result<(), MarketDataError> {
        let book = builder.to_order_book(depth, trace);
        if let Err(e) = book.validate() {
            self.counters.record_parse_error();
            warn!(dex = %Dex::Aster, symbol = %book.symbol, error = %e, "板の整合性チェックに失敗");
            return Ok(());
        }
        if tx.capacity() == 0 {
            warn!(dex = %Dex::Aster, "板チャネルが飽和（下流の処理が追いついていない）");
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
        let streams: Vec<String> = symbols
            .iter()
            .map(|s| self.cfg.depth_stream_name(*s))
            .collect();
        let url = combined_stream_url(&self.cfg.ws_url, &streams);

        let (ws, _resp) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .map_err(|e| MarketDataError::WebSocket(e.to_string()))?;

        self.state.set_connected();
        backoff.reset();
        info!(dex = %Dex::Aster, streams = streams.len(), "WS 接続確立");

        let (mut write, mut read) = ws.split();

        let max_levels = (self.cfg.snapshot_limit as usize).clamp(depth, MAX_TRACKED_LEVELS);
        let mut builders: HashMap<Symbol, AsterBookBuilder> = symbols
            .iter()
            .map(|s| (*s, AsterBookBuilder::new(*s, max_levels)))
            .collect();

        let (snap_tx, mut snap_rx) = mpsc::channel::<SnapshotResult>(SNAPSHOT_CHANNEL_CAPACITY);
        let mut pending: HashSet<Symbol> = HashSet::new();
        let mut next_snapshot_at = Instant::now();

        // 手順1→2: 購読を開始した状態でスナップショットを取りに行く。
        // 到着までの差分は各 builder がバッファする。
        for symbol in symbols {
            self.schedule_snapshot(*symbol, &mut next_snapshot_at, &mut pending, &snap_tx);
        }

        // 24 時間で強制切断されるため、その前に能動的に張り直す。
        let session_deadline = self
            .cfg
            .session_max_duration()
            .map(|d| Instant::now() + d)
            // 無効時は事実上発火しない時刻にしておく
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(365 * 24 * 3_600));

        loop {
            tokio::select! {
                incoming = read.next() => {
                    let Some(msg) = incoming else {
                        warn!(dex = %Dex::Aster, "WS ストリームが終了");
                        return Ok(());
                    };
                    let msg = msg.map_err(|e| MarketDataError::WebSocket(e.to_string()))?;
                    match msg {
                        Message::Text(text) => {
                            // パース前に受信時刻を確定させる（レイテンシ計測の起点）
                            let trace = MessageTrace::on_receive();
                            self.counters.record_message();
                            self.handle_text(
                                text.as_str(),
                                depth,
                                trace,
                                &mut builders,
                                &mut next_snapshot_at,
                                &mut pending,
                                &snap_tx,
                                tx,
                            )
                            .await?;
                        }
                        // Aster は 5 分ごとにサーバーから ping frame を送る。
                        // 15 分以内に pong を返さないと切断される。
                        Message::Ping(payload) => {
                            write
                                .send(Message::Pong(payload))
                                .await
                                .map_err(|e| MarketDataError::WebSocket(e.to_string()))?;
                        }
                        Message::Close(frame) => {
                            warn!(dex = %Dex::Aster, ?frame, "WS クローズを受信");
                            return Ok(());
                        }
                        Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {}
                    }
                }
                Some((symbol, result)) = snap_rx.recv() => {
                    pending.remove(&symbol);
                    match result {
                        Ok(snapshot) => {
                            let Some(builder) = builders.get_mut(&symbol) else { continue };
                            let trace = MessageTrace::on_receive();
                            match builder.apply_snapshot(&snapshot) {
                                Ok(()) => {
                                    self.counters.record_resync();
                                    info!(
                                        dex = %Dex::Aster,
                                        symbol = %symbol,
                                        last_update_id = snapshot.last_update_id,
                                        "板をスナップショットから初期化"
                                    );
                                    let builder = &builders[&symbol];
                                    self.emit(builder, depth, trace, tx).await?;
                                }
                                Err(e) => {
                                    // バッファ済み差分と繋がらなかった。取り直す。
                                    self.counters.record_sequence_gap();
                                    warn!(
                                        dex = %Dex::Aster,
                                        symbol = %symbol,
                                        error = %e,
                                        "スナップショットが差分と繋がりません。取り直します"
                                    );
                                    if let Some(b) = builders.get_mut(&symbol) {
                                        b.reset();
                                    }
                                    self.schedule_snapshot(
                                        symbol,
                                        &mut next_snapshot_at,
                                        &mut pending,
                                        &snap_tx,
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            warn!(
                                dex = %Dex::Aster,
                                symbol = %symbol,
                                error = %e,
                                "スナップショット取得に失敗。バックオフ後に再試行します"
                            );
                            self.schedule_snapshot(
                                symbol,
                                &mut next_snapshot_at,
                                &mut pending,
                                &snap_tx,
                            );
                        }
                    }
                }
                _ = tokio::time::sleep_until(session_deadline.into()) => {
                    info!(
                        dex = %Dex::Aster,
                        hours = self.cfg.reconnect_before_hours,
                        "24 時間の強制切断を待たずに再接続します"
                    );
                    return Ok(());
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
        builders: &mut HashMap<Symbol, AsterBookBuilder>,
        next_snapshot_at: &mut Instant,
        pending: &mut HashSet<Symbol>,
        snap_tx: &mpsc::Sender<SnapshotResult>,
        tx: &mpsc::Sender<OrderBook>,
    ) -> Result<(), MarketDataError> {
        let event = match parse_depth_message(text) {
            Ok(Some(ev)) => ev,
            // 購読応答など depth 以外のメッセージ
            Ok(None) => return Ok(()),
            Err(e) => {
                self.counters.record_parse_error();
                warn!(
                    dex = %Dex::Aster,
                    error = %e,
                    snippet = %text.chars().take(200).collect::<String>(),
                    "メッセージのパースに失敗"
                );
                return Ok(());
            }
        };

        let Some(symbol) = event.symbol() else {
            debug!(dex = %Dex::Aster, symbol_raw = %event.symbol_raw, "未購読のシンボルを無視");
            return Ok(());
        };
        let Some(builder) = builders.get_mut(&symbol) else {
            return Ok(());
        };

        match builder.apply_event(&event) {
            Ok(ApplyOutcome::Applied) => {
                let builder = &builders[&symbol];
                self.emit(builder, depth, trace, tx).await?;
            }
            Ok(ApplyOutcome::Buffered | ApplyOutcome::Discarded) => {}
            Err(e) => {
                self.counters.record_sequence_gap();
                warn!(
                    dex = %Dex::Aster,
                    symbol = %symbol,
                    error = %e,
                    "差分の連続性が崩れました。板を作り直します"
                );
                builder.reset();
                self.schedule_snapshot(symbol, next_snapshot_at, pending, snap_tx);
            }
        }
        Ok(())
    }
}

#[async_trait]
impl MarketDataSource for AsterMarketData {
    fn dex(&self) -> Dex {
        Dex::Aster
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
                    warn!(dex = %Dex::Aster, "セッションが終了しました");
                }
                Err(MarketDataError::ChannelClosed) => {
                    self.state.set_disconnected();
                    info!(dex = %Dex::Aster, "下流チャネルが閉じたため購読を終了");
                    return Err(MarketDataError::ChannelClosed);
                }
                Err(e) => {
                    warn!(dex = %Dex::Aster, error = %e, "セッションが異常終了");
                }
            }

            match backoff.next_delay() {
                Some(delay) => {
                    self.state.set_reconnecting(backoff.attempt());
                    info!(
                        dex = %Dex::Aster,
                        attempt = backoff.attempt(),
                        delay_ms = delay.as_millis() as u64,
                        "再接続を待機"
                    );
                    tokio::time::sleep(delay).await;
                }
                None => {
                    self.state.set_disconnected();
                    error!(dex = %Dex::Aster, attempts = backoff.attempt(), "再接続の上限に到達");
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
