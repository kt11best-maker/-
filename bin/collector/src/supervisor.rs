use std::sync::Arc;
use std::time::Duration;

use core_types::FundingRate;
use core_types::OrderBook;
use core_types::Symbol;
use dex_traits::{FundingRateSource, MarketDataError, MarketDataSource};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

/// タスクが落ちてから再起動するまでの待ち時間。
///
/// DEX クライアント自身が指数バックオフで再接続を試みたうえで諦めた後の
/// 再起動なので、ここは固定間隔でよい。
const RESTART_DELAY: Duration = Duration::from_secs(5);

/// Market Data タスクを監視し、異常終了・panic から復帰させる。
///
/// 1 つの DEX が落ちても収集全体を止めないための supervisor。内側のタスクを
/// `tokio::spawn` して `JoinHandle` を await することで、panic も検知できる。
pub fn spawn_supervised_source(
    source: Arc<dyn MarketDataSource>,
    symbols: Vec<Symbol>,
    depth: usize,
    tx: mpsc::Sender<OrderBook>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let dex = source.dex();
        let mut restarts: u64 = 0;

        loop {
            if *shutdown.borrow() {
                break;
            }

            let inner_source = Arc::clone(&source);
            let inner_symbols = symbols.clone();
            let inner_tx = tx.clone();
            let handle = tokio::spawn(async move {
                inner_source
                    .subscribe_orderbooks(&inner_symbols, depth, inner_tx)
                    .await
            });

            let outcome = tokio::select! {
                joined = handle => Some(joined),
                _ = shutdown.changed() => None,
            };

            match outcome {
                // shutdown が来た（select! で handle は drop され、タスクは中断される）
                None => break,
                Some(Ok(Ok(()))) => {
                    info!(dex = %dex, "Market Data タスクが正常終了");
                    break;
                }
                Some(Ok(Err(MarketDataError::ChannelClosed))) => {
                    info!(dex = %dex, "下流が閉じたため Market Data タスクを終了");
                    break;
                }
                Some(Ok(Err(e))) => {
                    error!(dex = %dex, error = %e, restarts, "Market Data タスクが異常終了");
                }
                Some(Err(join_error)) => {
                    // panic した場合。他の DEX の収集を巻き込まないようここで吸収する。
                    error!(dex = %dex, error = %join_error, restarts, "Market Data タスクが panic");
                }
            }

            restarts += 1;
            warn!(dex = %dex, restarts, delay_secs = RESTART_DELAY.as_secs(), "タスクを再起動します");

            tokio::select! {
                _ = tokio::time::sleep(RESTART_DELAY) => {}
                _ = shutdown.changed() => break,
            }
        }

        info!(dex = %dex, restarts, "supervisor 終了");
    })
}

/// ファンディング収集タスクを起動する。
///
/// **板の収集より優先度が低い。** ここが失敗しても板は止めないため、
/// 板側のような再起動ループは持たず、失敗をログに残して終了する。
/// レート自体は板と同じ WS 接続から流れてくるので、接続の復旧は板側の
/// supervisor が面倒を見る。
pub fn spawn_funding_source(
    source: Arc<dyn FundingRateSource>,
    symbols: Vec<Symbol>,
    tx: mpsc::Sender<FundingRate>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let dex = source.dex();
        match source.subscribe_funding(&symbols, tx).await {
            Ok(()) => info!(dex = %dex, "ファンディング収集タスクが正常終了"),
            Err(e) => warn!(
                dex = %dex,
                error = %e,
                "ファンディング収集タスクが終了（板の収集は継続します）"
            ),
        }
    })
}
