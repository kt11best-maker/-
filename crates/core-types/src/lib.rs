//! フェーズ1で確定させる共通型。
//!
//! 設計方針:
//! - 数値は必ず [`rust_decimal::Decimal`]。`f64` は丸め誤差のため使用しない。
//! - 価格と数量は newtype で包み、取り違えをコンパイル時に防ぐ。
//! - 時刻は「取引所との比較 = wall clock」「自プロセス内の区間計測 = monotonic
//!   ([`std::time::Instant`])」を厳密に使い分ける（[`MessageTrace`] を参照）。

pub mod book;
pub mod fees;
pub mod funding;
pub mod num;
pub mod symbol;
pub mod trace;

pub use book::{Level, OrderBook, Side};
pub use fees::{DexFees, ExecutionStyle, FeeSchedule};
pub use funding::{breakeven_intervals, FundingRate, FundingSpread};
pub use num::{Price, Quantity, BPS_DENOMINATOR};
pub use symbol::{Dex, ParseSymbolError, Symbol};
pub use trace::MessageTrace;

/// UNIX epoch からの経過ミリ秒（wall clock）。
///
/// 取引所タイムスタンプとの比較にのみ使う。自プロセス内の区間計測には使わない。
pub fn now_wall_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
