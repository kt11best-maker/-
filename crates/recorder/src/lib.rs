//! ログ初期化と、価格差スナップショットの CSV 出力。

pub mod csv_writer;
pub mod logging;

pub use csv_writer::{spawn_csv_writer, CsvRecorder, CSV_HEADER};
pub use logging::init_logging;
