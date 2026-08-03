use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use dex_edgex::EdgeXMarketData;
use dex_hyperliquid::HyperliquidMarketData;
use dex_traits::{ConnectionStatus, MarketDataSource};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::aggregator::AggregatorStats;

/// 監視対象のハンドル。
#[derive(Clone, Default)]
pub struct MonitorTargets {
    pub hyperliquid: Option<Arc<HyperliquidMarketData>>,
    pub edgex: Option<Arc<EdgeXMarketData>>,
}

/// 接続状態とスループットを定期出力する監視タスク。
///
/// 24 時間稼働の健全性（受信が止まっていないか、再接続を繰り返していないか、
/// 板を捨てていないか）をログだけで追えるようにするのが目的。
pub fn spawn_monitor(
    interval_secs: u64,
    targets: MonitorTargets,
    stats: Arc<AggregatorStats>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs.max(1)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;

        // 前回時点の受信数。差分を取って「止まっていないか」を見る。
        let mut prev_hl_messages = 0u64;
        let mut prev_edgex_messages = 0u64;

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
                _ = ticker.tick() => {
                    if let Some(hl) = targets.hyperliquid.as_ref() {
                        let received = hl.messages_received();
                        report_dex(
                            "hyperliquid",
                            hl.connection_status(),
                            received,
                            received.saturating_sub(prev_hl_messages),
                            hl.books_emitted(),
                            hl.parse_errors(),
                            None,
                            interval_secs,
                        );
                        prev_hl_messages = received;
                    }
                    if let Some(edgex) = targets.edgex.as_ref() {
                        let received = edgex.messages_received();
                        report_dex(
                            "edgex",
                            edgex.connection_status(),
                            received,
                            received.saturating_sub(prev_edgex_messages),
                            edgex.books_emitted(),
                            edgex.parse_errors(),
                            Some(edgex.sequence_gaps()),
                            interval_secs,
                        );
                        prev_edgex_messages = received;
                    }

                    info!(
                        books_processed = stats.books_processed.load(Ordering::Relaxed),
                        snapshots_computed = stats.snapshots_computed.load(Ordering::Relaxed),
                        snapshots_dropped = stats.snapshots_dropped.load(Ordering::Relaxed),
                        latency_warnings = stats.latency_warnings.load(Ordering::Relaxed),
                        staleness_warnings = stats.staleness_warnings.load(Ordering::Relaxed),
                        clock_skew_warnings = stats.clock_skew_warnings.load(Ordering::Relaxed),
                        "パイプライン統計"
                    );
                }
            }
        }
        info!("監視タスク終了");
    })
}

#[allow(clippy::too_many_arguments)]
fn report_dex(
    dex: &str,
    status: ConnectionStatus,
    messages_total: u64,
    messages_delta: u64,
    books_emitted: u64,
    parse_errors: u64,
    sequence_gaps: Option<u64>,
    interval_secs: u64,
) {
    info!(
        dex,
        status = %status,
        messages_total,
        messages_delta,
        books_emitted,
        parse_errors,
        sequence_gaps,
        "接続状態"
    );

    if !status.is_connected() {
        warn!(dex, status = %status, "DEX に接続できていません");
    } else if messages_delta == 0 {
        // 接続は生きているのにデータが来ていない = 購読が通っていない可能性。
        warn!(
            dex,
            interval_secs, "接続中だが直近の受信が 0 件（購読状態を確認してください）"
        );
    }
}
