//! 板の集約と価格差計算。
//!
//! 板が更新されるたびに（= イベント駆動で）価格差を再計算する。定期ポーリングにすると
//! 「乖離が発生してから解消されるまでの時間」の計測解像度が落ちるため。

pub mod divergence;
pub mod store;

pub use divergence::{DivergenceSnapshot, ExecutableDirection, DEPTH_SLIPPAGE_BPS};
pub use store::BookStore;
