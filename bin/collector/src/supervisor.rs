use std::sync::Arc;
use std::time::Duration;

use core_types::OrderBook;
use core_types::Symbol;
use dex_traits::{MarketDataError, MarketDataSource};
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
