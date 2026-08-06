//! 戦略の共通インターフェース。
//!
//! 価格差アービトラージとファンディング裁定は、収益源も速度要求もポジション
//! 保有時間も違うが、**同じ配管（板データ・執行基盤・リスク管理）を共有する**。
//! そのための共通の入口がこの crate。
//!
//! | 観点 | 価格差アービトラージ | ファンディング裁定 |
//! |---|---|---|
//! | 収益源 | DEX 間の一時的な価格乖離 | DEX 間のファンディングレート差 |
//! | 速度要求 | 極めて高い（数十〜数百 ms） | 低い（数分〜数時間） |
//! | 保有時間 | 秒〜分 | 時間〜日 |
//! | 執行方式 | IOC 同時発注、即時クローズ | 指値でじっくり建てられる |
//! | キャパシティ | 板の厚さに強く制約 | 証拠金量に制約 |

pub mod context;
pub mod signal;

pub use context::MarketContext;
pub use signal::{ExitReason, OpenPosition, SignalRationale, StrategyKind, TradeSignal, Urgency};

/// 戦略の共通インターフェース。
///
/// 判定は**純粋関数**にしてある（I/O を持たない）。同じ市場状態を渡せば同じ
/// シグナルが出るので、フェーズ2 のドライランで収集済みデータを流し直して
/// 事後評価できる。
pub trait Strategy: Send + Sync {
    fn kind(&self) -> StrategyKind;

    /// 現在の市場状態からシグナルを評価する。
    fn evaluate(&self, ctx: &MarketContext<'_>) -> Vec<TradeSignal>;

    /// 既存ポジションを解消すべきか判定する。
    fn should_exit(&self, position: &OpenPosition, ctx: &MarketContext<'_>) -> Option<ExitReason>;
}
