use std::io;

use config::RecordingConfig;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// JSON 形式のファイルログを初期化する。
///
/// 返り値の [`WorkerGuard`] は **main の終了まで保持し続けること**。drop すると
/// バックグラウンドの書き込みワーカーが停止し、末尾のログが失われる。
///
/// `RUST_LOG` が設定されていればそちらを優先し、無ければ設定ファイルの
/// `recording.log_filter` を使う。
pub fn init_logging(cfg: &RecordingConfig) -> io::Result<WorkerGuard> {
    std::fs::create_dir_all(&cfg.log_dir)?;

    let file_appender = tracing_appender::rolling::daily(&cfg.log_dir, "collector.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&cfg.log_filter))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(true)
        .json();

    let registry = tracing_subscriber::registry().with(filter).with(file_layer);

    if cfg.log_to_stdout {
        // 運用中の目視確認用。ファイル側は JSON のまま維持する。
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(io::stdout)
                    .with_target(false)
                    .compact(),
            )
            .init();
    } else {
        registry.init();
    }

    Ok(guard)
}
