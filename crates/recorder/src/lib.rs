//! ログ初期化と CSV 出力。
//!
//! CSV は 3 系統ある。**混ぜないこと。**
//!
//! | 系統 | ファイル | 1 行の意味 | 更新頻度 |
//! |---|---|---|---|
//! | 価格差 | `{date}_{SYMBOL}.csv` | 1 ペアの比較 | 数十〜数百 ms |
//! | ファンディング | `{date}_{SYMBOL}_funding.csv` | 1 DEX の状態 | 数秒〜数分 |
//! | ネットデルタ | `{date}_net_delta.csv` | 1 銘柄の照合結果 | 数十秒〜数分 |
//!
//! 更新頻度が桁違いなので、同じファイルに混ぜるとほぼ同じ値が大量に重複する。
//! 流動性指標（OI・出来高）だけは例外で、**ファンディングと同じメッセージで
//! 届く**ためファンディング CSV の同じ行に相乗りさせている。

pub mod csv_writer;
pub mod funding_csv;
pub mod logging;
pub mod net_delta_csv;

pub use csv_writer::{spawn_csv_writer, CsvRecorder, CSV_HEADER};
pub use funding_csv::{spawn_funding_csv_writer, FundingRecorder, FUNDING_CSV_HEADER};
pub use logging::init_logging;
pub use net_delta_csv::{net_delta_csv_header, spawn_net_delta_csv_writer, NetDeltaRecorder};
