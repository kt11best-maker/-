use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use config::Config;
use core_types::{Dex, OrderBook};
use market_data::{BookStore, DivergenceSnapshot};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::rate_limit::RateLimiter;

/// 集約タスクの統計。監視タスクから読む。
#[derive(Debug, Default)]
pub struct AggregatorStats {
    pub books_processed: AtomicU64,
    pub snapshots_computed: AtomicU64,
    /// CSV チャネルが詰まって捨てたスナップショット数。
    pub snapshots_dropped: AtomicU64,
    pub latency_warnings: AtomicU64,
    pub staleness_warnings: AtomicU64,
    pub clock_skew_warnings: AtomicU64,
}

/// 板更新をトリガーに価格差を計算し、CSV writer へ渡すタスク。
///
/// 定期ポーリングではなくイベント駆動にしているのは、乖離の発生から解消までの
/// 継続時間（フェーズ2 の判断材料として最重要）を取りこぼさないため。
pub fn spawn_aggregator(
    cfg: &Config,
    store: Arc<BookStore>,
    stats: Arc<AggregatorStats>,
    mut book_rx: mpsc::Receiver<OrderBook>,
    snapshot_tx: mpsc::Sender<DivergenceSnapshot>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let vwap_notional = cfg.vwap_notional();
    let latency_warn_ms = cfg.monitoring.latency_warn_threshold_ms;
    let staleness_warn_ms = cfg.monitoring.staleness_warn_threshold_ms;
    let mut limiter = RateLimiter::new(Duration::from_secs(
        cfg.monitoring.warn_min_interval_secs.max(1),
    ));

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
                incoming = book_rx.recv() => {
                    let Some(book) = incoming else { break };
                    let trigger = book.dex;
                    let symbol = book.symbol;

                    check_latency(&book, latency_warn_ms, &mut limiter, &stats);

                    store.update(book);
                    stats.books_processed.fetch_add(1, Ordering::Relaxed);

                    // 比較の向きは常に (A=Hyperliquid, B=edgeX) に固定する。
                    // CSV の符号の意味が行ごとに変わらないようにするため。
                    let Some(snapshot) = store.divergence(
                        symbol,
                        Dex::Hyperliquid,
                        Dex::EdgeX,
                        trigger,
                        vwap_notional,
                    ) else {
                        continue;
                    };
                    stats.snapshots_computed.fetch_add(1, Ordering::Relaxed);

                    check_staleness(&snapshot, staleness_warn_ms, &mut limiter, &stats);

                    // ディスク I/O の詰まりが受信側に伝播しないよう、ここはノンブロッキング。
                    // 捨てた件数は必ず記録し、欠測を後から把握できるようにする。
                    if snapshot_tx.try_send(snapshot).is_err() {
                        let dropped = stats.snapshots_dropped.fetch_add(1, Ordering::Relaxed) + 1;
                        if let Some(suppressed) = limiter.allow("csv_backpressure") {
                            warn!(
                                dropped_total = dropped,
                                suppressed,
                                "CSV チャネルが詰まったためスナップショットを破棄"
                            );
                        }
                    }
                }
            }
        }
        info!(
            books_processed = stats.books_processed.load(Ordering::Relaxed),
            snapshots_computed = stats.snapshots_computed.load(Ordering::Relaxed),
            snapshots_dropped = stats.snapshots_dropped.load(Ordering::Relaxed),
            "集約タスク終了"
        );
    })
}

/// 取引所 → 受信の遅延と時計ズレを検査する。
fn check_latency(
    book: &OrderBook,
    warn_threshold_ms: i64,
    limiter: &mut RateLimiter,
    stats: &AggregatorStats,
) {
    let Some(latency_ms) = book.trace.exchange_to_local_ms() else {
        return;
    };

    if latency_ms < 0 {
        // 取引所の時刻が自分より未来 = NTP 未同期などの時計ズレ。
        // レイテンシ計測そのものが信用できなくなるため必ず気づけるようにする。
        stats.clock_skew_warnings.fetch_add(1, Ordering::Relaxed);
        if let Some(suppressed) = limiter.allow(&format!("skew:{}", book.dex)) {
            warn!(
                dex = %book.dex,
                symbol = %book.symbol,
                latency_ms,
                suppressed,
                "取引所→受信の遅延が負。時計ズレの疑い（NTP 同期を確認）"
            );
        }
    } else if latency_ms > warn_threshold_ms {
        stats.latency_warnings.fetch_add(1, Ordering::Relaxed);
        if let Some(suppressed) = limiter.allow(&format!("latency:{}", book.dex)) {
            warn!(
                dex = %book.dex,
                symbol = %book.symbol,
                latency_ms,
                threshold_ms = warn_threshold_ms,
                suppressed,
                "取引所→受信の遅延が閾値超過"
            );
        }
    }
}

/// 両 DEX の板の鮮度差を検査する。
fn check_staleness(
    snapshot: &DivergenceSnapshot,
    warn_threshold_ms: i64,
    limiter: &mut RateLimiter,
    stats: &AggregatorStats,
) {
    if snapshot.staleness_delta_ms.abs() <= warn_threshold_ms {
        return;
    }
    stats.staleness_warnings.fetch_add(1, Ordering::Relaxed);
    if let Some(suppressed) = limiter.allow(&format!("staleness:{}", snapshot.symbol)) {
        warn!(
            symbol = %snapshot.symbol,
            staleness_delta_ms = snapshot.staleness_delta_ms,
            threshold_ms = warn_threshold_ms,
            raw_spread_bps = %snapshot.raw_spread_bps,
            suppressed,
            "両 DEX の板の鮮度差が大きい（見かけ上の乖離の可能性）"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{Level, MessageTrace, Price, Quantity, Symbol};
    use rust_decimal_macros::dec;

    fn book_with_latency(
        dex: Dex,
        exchange_ts_ms: Option<u64>,
        received_wall_ms: u64,
    ) -> OrderBook {
        let mut trace = MessageTrace::on_receive();
        trace.exchange_ts_ms = exchange_ts_ms;
        trace.received_wall_ms = received_wall_ms;
        OrderBook::new(
            dex,
            Symbol::Btc,
            vec![Level::new(Price(dec!(100)), Quantity(dec!(1)))],
            vec![Level::new(Price(dec!(101)), Quantity(dec!(1)))],
            trace,
        )
    }

    #[test]
    fn flags_slow_messages() {
        let stats = AggregatorStats::default();
        let mut limiter = RateLimiter::new(Duration::from_secs(0));
        check_latency(
            &book_with_latency(Dex::Hyperliquid, Some(1_000), 1_600),
            500,
            &mut limiter,
            &stats,
        );
        assert_eq!(stats.latency_warnings.load(Ordering::Relaxed), 1);
        assert_eq!(stats.clock_skew_warnings.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn flags_negative_latency_as_clock_skew() {
        let stats = AggregatorStats::default();
        let mut limiter = RateLimiter::new(Duration::from_secs(0));
        check_latency(
            &book_with_latency(Dex::EdgeX, Some(2_000), 1_900),
            500,
            &mut limiter,
            &stats,
        );
        assert_eq!(stats.clock_skew_warnings.load(Ordering::Relaxed), 1);
        assert_eq!(stats.latency_warnings.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn ignores_healthy_and_timestampless_messages() {
        let stats = AggregatorStats::default();
        let mut limiter = RateLimiter::new(Duration::from_secs(0));
        check_latency(
            &book_with_latency(Dex::Hyperliquid, Some(1_000), 1_050),
            500,
            &mut limiter,
            &stats,
        );
        check_latency(
            &book_with_latency(Dex::Hyperliquid, None, 1_050),
            500,
            &mut limiter,
            &stats,
        );
        assert_eq!(stats.latency_warnings.load(Ordering::Relaxed), 0);
        assert_eq!(stats.clock_skew_warnings.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn flags_stale_pairs_in_both_directions() {
        let stats = AggregatorStats::default();
        let mut limiter = RateLimiter::new(Duration::from_secs(0));
        let a = book_with_latency(Dex::Hyperliquid, Some(1_000), 5_000);
        let b = book_with_latency(Dex::EdgeX, Some(1_000), 1_000);

        let snap = DivergenceSnapshot::compute(&a, &b, Dex::Hyperliquid, None).unwrap();
        check_staleness(&snap, 1_000, &mut limiter, &stats);
        let flipped = DivergenceSnapshot::compute(&b, &a, Dex::EdgeX, None).unwrap();
        check_staleness(&flipped, 1_000, &mut limiter, &stats);
        assert_eq!(stats.staleness_warnings.load(Ordering::Relaxed), 2);

        // 閾値内なら何も出ない
        let fresh = DivergenceSnapshot::compute(&b, &b.clone(), Dex::EdgeX, None);
        assert!(fresh.is_none(), "同一 DEX の比較は成立しない");
    }
}
