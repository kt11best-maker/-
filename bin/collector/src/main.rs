//! フェーズ1 の実行バイナリ。
//!
//! 有効化された DEX（Hyperliquid / edgeX / Aster / Lighter）の板を購読し、
//! 価格差スナップショットを CSV に、運用イベントを JSON ログに書き出す。
//! **発注は一切行わない。**
//!
//! ```text
//! 起動
//!  ├─ config 読み込み
//!  ├─ ログ初期化（tracing → JSON file）
//!  ├─ BookStore 初期化
//!  ├─ spawn: 各 DEX の WS 購読タスク（有効なものだけ）─→ mpsc<OrderBook>
//!  ├─ spawn: 集約タスク（N(N-1)/2 ペアの価格差）      ─→ mpsc<DivergenceSnapshot>
//!  ├─ spawn: CSV writer タスク（バッファリングして定期 flush）
//!  ├─ spawn: ファンディング収集タスク（対応 DEX のみ） ─→ mpsc<FundingRate>
//!  ├─ spawn: ファンディング CSV writer タスク
//!  ├─ spawn: 監視タスク（接続状態・レイテンシ異常）
//!  └─ SIGINT/SIGTERM → CSV を flush して正常終了
//! ```
//!
//! ファンディングは板とは**独立したパイプライン**。取得に失敗しても板の収集は
//! 止まらない（フェーズ1 の主目的は板データの収集）。

mod aggregator;
mod monitor;
mod rate_limit;
mod supervisor;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use config::Config;
use core_types::Dex;
use dex_aster::AsterMarketData;
use dex_dydx::DydxMarketData;
use dex_edgex::EdgeXMarketData;
use dex_hyperliquid::HyperliquidMarketData;
use dex_lighter::LighterMarketData;
use dex_traits::{FundingRateSource, MarketDataSource};
use market_data::BookStore;
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

use crate::aggregator::{spawn_aggregator, AggregatorStats};
use crate::monitor::spawn_monitor;
use crate::supervisor::{spawn_funding_source, spawn_supervised_source};

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
    let sources = build_sources(&cfg)?;
    let mut source_handles = Vec::new();

    for source in &sources.market_data {
        let dex = source.dex();
        let symbols = cfg.symbols_for(dex);
        info!(
            dex = %dex,
            symbols = ?symbols.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "Market Data タスクを起動"
        );
        source_handles.push(spawn_supervised_source(
            Arc::clone(source),
            symbols,
            cfg.general.orderbook_depth,
            book_tx.clone(),
            shutdown_rx.clone(),
        ));
    }

    // 送信側の原本は落としておく。全 source タスクが終われば集約タスクも終わる。
    drop(book_tx);

    // --- ファンディング収集（板とは独立したパイプライン）---
    let mut funding_handles = Vec::new();
    let funding_csv_handle = if cfg.funding.enabled && !sources.funding.is_empty() {
        let (funding_tx, funding_rx) = mpsc::channel(cfg.funding.channel_capacity);
        for source in &sources.funding {
            let dex = source.dex();
            info!(dex = %dex, "ファンディング収集タスクを起動");
            funding_handles.push(spawn_funding_source(
                Arc::clone(source),
                cfg.symbols_for(dex),
                funding_tx.clone(),
            ));
        }
        drop(funding_tx);

        if cfg.recording.funding.enabled {
            Some(recorder::spawn_funding_csv_writer(
                &cfg.recording,
                funding_rx,
            ))
        } else {
            info!("recording.funding.enabled = false のため CSV には残しません");
            None
        }
    } else {
        if cfg.funding.enabled {
            warn!("ファンディングに対応した DEX が有効化されていません");
        } else {
            info!("funding.enabled = false のためファンディングは収集しません");
        }
        None
    };

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
        sources.market_data.clone(),
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

    // ファンディング側も同様に flush する。収集タスクを止めると送信側が
    // 全て drop され、CSV writer がループを抜けて flush する。
    for handle in funding_handles {
        handle.abort();
    }
    if let Some(handle) = funding_csv_handle {
        match tokio::time::timeout(SHUTDOWN_GRACE, handle).await {
            Ok(Ok(())) => info!("ファンディング CSV を flush して終了しました"),
            Ok(Err(e)) => error!(error = %e, "ファンディング CSV writer が異常終了"),
            Err(_) => error!(
                timeout_secs = SHUTDOWN_GRACE.as_secs(),
                "ファンディング CSV の flush がタイムアウトしました"
            ),
        }
    }

    monitor_handle.abort();
    for handle in source_handles {
        handle.abort();
    }

    info!("collector 終了");
    Ok(())
}

/// 設定で有効化された DEX のソース群。
struct Sources {
    market_data: Vec<Arc<dyn MarketDataSource>>,
    /// ファンディングにも対応している DEX だけが入る。
    /// **全 DEX が同じ方式でファンディングを提供するとは限らない。**
    funding: Vec<Arc<dyn FundingRateSource>>,
}

/// 設定で有効化された DEX の Market Data ソースを構築する。
///
/// DEX を追加する場合はここに 1 分岐足すだけでよい（以降のパイプラインは
/// [`MarketDataSource`] の trait 越しに扱うため変更不要）。
fn build_sources(cfg: &Config) -> Result<Sources> {
    let mut market_data: Vec<Arc<dyn MarketDataSource>> = Vec::new();
    let mut funding: Vec<Arc<dyn FundingRateSource>> = Vec::new();
    let collect_funding = cfg.funding.enabled;

    if cfg.dex.hyperliquid.enabled {
        // ファンディング（activeAssetCtx）は板と同じ接続に相乗りする
        let source = Arc::new(
            HyperliquidMarketData::new(cfg.dex.hyperliquid.clone()).with_funding(
                collect_funding,
                cfg.funding_interval_hours(Dex::Hyperliquid),
            ),
        );
        market_data.push(Arc::clone(&source) as Arc<dyn MarketDataSource>);
        if collect_funding {
            funding.push(source as Arc<dyn FundingRateSource>);
        }
    }
    if cfg.dex.edgex.enabled {
        // edgeX のファンディング取得方法は未確認のため、板のみ収集する
        market_data.push(Arc::new(EdgeXMarketData::new(cfg.dex.edgex.clone())));
    }
    if cfg.dex.aster.enabled {
        // Aster は @markPrice ストリームで取れる見込みだが、1 秒 10 メッセージ
        // 制限の計算が必要なため未実装（板のみ収集する）
        market_data.push(Arc::new(
            AsterMarketData::new(cfg.dex.aster.clone())
                .context("Aster クライアントの初期化に失敗")?,
        ));
    }
    if cfg.dex.lighter.enabled {
        // ファンディングは既存の market_stats 購読から分岐する（接続は増えない）
        let source = Arc::new(
            LighterMarketData::new(cfg.dex.lighter.clone())
                .with_funding(collect_funding, cfg.funding_interval_hours(Dex::Lighter)),
        );
        market_data.push(Arc::clone(&source) as Arc<dyn MarketDataSource>);
        if collect_funding {
            funding.push(source as Arc<dyn FundingRateSource>);
        }
    }
    if cfg.dex.dydx.enabled {
        // dYdX のファンディング取得は未実装（板のみ）。
        // クロスした板が正常に起こるため、乖離判定側での除外が前提。
        market_data.push(Arc::new(DydxMarketData::new(cfg.dex.dydx.clone())));
    }

    for dex in Dex::ALL {
        if !cfg.is_enabled(dex) {
            warn!(dex = %dex, "設定で無効化されています");
        }
    }

    if collect_funding {
        warn_about_funding_gaps(cfg, &funding);
    }

    Ok(Sources {
        market_data,
        funding,
    })
}

/// ファンディング収集の穴を起動時に一度だけ報告する。
///
/// 「取れない DEX がある」「精算間隔が未設定で年率換算できない」はどちらも
/// 正常系だが、後から CSV を見て気づくのでは遅いのでここで出しておく。
fn warn_about_funding_gaps(cfg: &Config, funding: &[Arc<dyn FundingRateSource>]) {
    let supported: Vec<Dex> = funding.iter().map(|s| s.dex()).collect();
    for dex in cfg.enabled_dexes() {
        if !supported.contains(&dex) {
            warn!(dex = %dex, "ファンディングレートの取得に対応していません（板のみ収集）");
        }
    }
    for dex in cfg.dexes_missing_funding_interval() {
        if supported.contains(&dex) {
            // 年率換算されないだけでレート自体は記録される
            warn!(
                dex = %dex,
                "funding.intervals_hours が未設定です。年率換算（annualized_pct）は空欄になります"
            );
        }
    }
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
