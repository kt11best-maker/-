use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use core_types::Dex;
use dex_traits::MarketDataSource;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::aggregator::AggregatorStats;

/// 接続状態とスループットを定期出力する監視タスク。
///
/// 24 時間稼働の健全性（受信が止まっていないか、再接続を繰り返していないか、
/// 板を捨てていないか）をログだけで追えるようにするのが目的。
///
/// DEX 固有の型には依存せず [`MarketDataSource`] の trait 越しに扱うため、
/// DEX を追加してもこのタスクは変更不要。
pub fn spawn_monitor(
    interval_secs: u64,
    sources: Vec<Arc<dyn MarketDataSource>>,
    stats: Arc<AggregatorStats>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs.max(1)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;

        // 前回時点の受信数。差分を取って「止まっていないか」を見る。
        let mut previous: HashMap<Dex, u64> = HashMap::new();

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
                _ = ticker.tick() => {
                    for source in &sources {
                        let dex = source.dex();
                        let status = source.connection_status();
                        let metrics = source.metrics();
                        let previous_total = previous.insert(dex, metrics.messages_received).unwrap_or(0);
                        let delta = metrics.messages_received.saturating_sub(previous_total);

                        info!(
                            dex = %dex,
                            status = %status,
                            messages_total = metrics.messages_received,
                            messages_delta = delta,
                            books_emitted = metrics.books_emitted,
                            parse_errors = metrics.parse_errors,
                            sequence_gaps = metrics.sequence_gaps,
                            resyncs = metrics.resyncs,
                            "接続状態"
                        );

                        if !status.is_connected() {
                            warn!(dex = %dex, status = %status, "DEX に接続できていません");
                        } else if delta == 0 {
                            // 接続は生きているのにデータが来ていない = 購読が通っていない可能性。
                            warn!(
                                dex = %dex,
                                interval_secs,
                                "接続中だが直近の受信が 0 件（購読状態を確認してください）"
                            );
                        }
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
