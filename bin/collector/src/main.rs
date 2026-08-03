//! フェーズ1 の実行バイナリ。
//!
//! Hyperliquid と edgeX の板を購読し、価格差スナップショットを CSV に、
//! 運用イベントを JSON ログに書き出す。**発注は一切行わない。**
//!
//! ```text
//! 起動
//!  ├─ config 読み込み
//!  ├─ ログ初期化（tracing → JSON file）
//!  ├─ BookStore 初期化
//!  ├─ spawn: Hyperliquid WS 購読タスク ─┐
//!  ├─ spawn: edgeX WS 購読タスク       ─┼→ mpsc<OrderBook>
//!  ├─ spawn: 集約タスク  ←─────────────┘  → mpsc<DivergenceSnapshot>
//!  ├─ spawn: CSV writer タスク（バッファリングして定期 flush）
//!  ├─ spawn: 監視タスク（接続状態・レイテンシ異常）
//!  └─ SIGINT/SIGTERM → CSV を flush して正常終了
//! ```

mod aggregator;
mod monitor;
mod rate_limit;
mod supervisor;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use config::Config;
use dex_edgex::EdgeXMarketData;
use dex_hyperliquid::HyperliquidMarketData;
use dex_traits::MarketDataSource;
use market_data::BookStore;
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

use crate::aggregator::{spawn_aggregator, AggregatorStats};
use crate::monitor::{spawn_monitor, MonitorTargets};
use crate::supervisor::spawn_supervised_source;

const DEFAULT_CONFIG_PATH: &str = "config/collector.toml";
/// 終了時に CSV の flush を待つ上限。
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> Result<()> {
    let config_path = parse_config_path()?;
    let cfg = Config::load(&config_path)
        .with_context(|| format!("設定ファイルの読み込みに失敗: {}", config_path.display()))?;

    // ログの guard は main の最後まで保持する（drop するとログが失われる）。
    let _log_guard = recorder::init_logging(&cfg.recording).context("ログの初期化に失敗")?;

    info!(
        config = %config_path.display(),
        symbols = ?cfg.general.symbols.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        depth = cfg.general.orderbook_depth,
        csv_mode = ?cfg.recording.csv_mode,
        "collector 起動（発注機能なし・データ収集のみ）"
    );

    let store = Arc::new(BookStore::new());
    let stats = Arc::new(AggregatorStats::default());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let (book_tx, book_rx) = mpsc::channel(cfg.general.book_channel_capacity);
    let (snapshot_tx, snapshot_rx) = mpsc::channel(cfg.recording.snapshot_channel_capacity);

    // --- Market Data タスク ---
    let mut targets = MonitorTargets::default();
    let mut source_handles = Vec::new();

    if cfg.dex.hyperliquid.enabled {
        let source = Arc::new(HyperliquidMarketData::new(cfg.dex.hyperliquid.clone()));
        targets.hyperliquid = Some(Arc::clone(&source));
        source_handles.push(spawn_supervised_source(
            source as Arc<dyn MarketDataSource>,
            cfg.general.symbols.clone(),
            cfg.general.orderbook_depth,
            book_tx.clone(),
            shutdown_rx.clone(),
        ));
    } else {
        warn!(dex = "hyperliquid", "設定で無効化されています");
    }

    if cfg.dex.edgex.enabled {
        let source = Arc::new(EdgeXMarketData::new(cfg.dex.edgex.clone()));
        targets.edgex = Some(Arc::clone(&source));
        source_handles.push(spawn_supervised_source(
            source as Arc<dyn MarketDataSource>,
            cfg.general.symbols.clone(),
            cfg.general.orderbook_depth,
            book_tx.clone(),
            shutdown_rx.clone(),
        ));
    } else {
        warn!(dex = "edgex", "設定で無効化されています");
    }

    // 送信側の原本は落としておく。全 source タスクが終われば集約タスクも終わる。
    drop(book_tx);

    // --- 集約 / 記録 / 監視タスク ---
    let csv_handle = recorder::spawn_csv_writer(&cfg.recording, snapshot_rx);
    let aggregator_handle = spawn_aggregator(
        &cfg,
        Arc::clone(&store),
        Arc::clone(&stats),
        book_rx,
        snapshot_tx,
        shutdown_rx.clone(),
    );
    let monitor_handle = spawn_monitor(
        cfg.monitoring.status_interval_secs,
        targets,
        Arc::clone(&stats),
        shutdown_rx.clone(),
    );

    // --- 終了シグナル待ち ---
    wait_for_shutdown_signal().await;
    info!("終了シグナルを受信。停止処理を開始します");
    let _ = shutdown_tx.send(true);

    // 集約タスクが終わると snapshot_tx が drop され、CSV writer が flush して終了する。
    if let Err(e) = aggregator_handle.await {
        error!(error = %e, "集約タスクの終了待ちに失敗");
    }
    match tokio::time::timeout(SHUTDOWN_GRACE, csv_handle).await {
        Ok(Ok(())) => info!("CSV を flush して終了しました"),
        Ok(Err(e)) => error!(error = %e, "CSV writer が異常終了"),
        Err(_) => error!(
            timeout_secs = SHUTDOWN_GRACE.as_secs(),
            "CSV の flush がタイムアウトしました"
        ),
    }

    monitor_handle.abort();
    for handle in source_handles {
        handle.abort();
    }

    info!("collector 終了");
    Ok(())
}

/// `--config <path>` を読む。指定が無ければ既定パス。
fn parse_config_path() -> Result<PathBuf> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut path = PathBuf::from(DEFAULT_CONFIG_PATH);
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--config" | "-c" => {
                let value = args
                    .get(i + 1)
                    .context("--config には設定ファイルのパスが必要です")?;
                path = PathBuf::from(value);
                i += 2;
            }
            "--help" | "-h" => {
                println!(
                    "collector — フェーズ1 Market Data 収集バイナリ\n\n\
                     USAGE:\n    collector [--config <path>]\n\n\
                     OPTIONS:\n    -c, --config <path>    設定ファイル（既定: {DEFAULT_CONFIG_PATH}）\n    \
                     -h, --help             このヘルプを表示\n\n\
                     ENV:\n    RUST_LOG               ログフィルタ（設定ファイルより優先）"
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("不明な引数: {other}（--help を参照）"),
        }
    }

    Ok(path)
}

/// SIGINT / SIGTERM を待つ。
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, "SIGTERM ハンドラの登録に失敗。SIGINT のみ待機します");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => info!("SIGINT を受信"),
            _ = sigterm.recv() => info!("SIGTERM を受信"),
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        info!("Ctrl-C を受信");
    }
}
