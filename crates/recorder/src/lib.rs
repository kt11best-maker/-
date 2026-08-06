//! ログ初期化と CSV 出力。
//!
//! CSV は 2 系統ある。**混ぜないこと。**
//!
//! | 系統 | ファイル | 1 行の意味 | 更新頻度 |
//! |---|---|---|---|
//! | 価格差 | `{date}_{SYMBOL}.csv` | 1 ペアの比較 | 数十〜数百 ms |
//! | ファンディング | `{date}_{SYMBOL}_funding.csv` | 1 DEX の状態 | 数秒〜数分 |
//!
//! 更新頻度が桁違いなので、同じファイルに混ぜるとほぼ同じ値が大量に重複する。

pub mod csv_writer;
pub mod funding_csv;
pub mod logging;

pub use csv_writer::{spawn_csv_writer, CsvRecorder, CSV_HEADER};
pub use funding_csv::{spawn_funding_csv_writer, FundingRecorder, FUNDING_CSV_HEADER};
pub use logging::init_logging;
