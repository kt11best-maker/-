//! 板の集約と価格差計算。
//!
//! 板が更新されるたびに（= イベント駆動で）価格差を再計算する。定期ポーリングにすると
//! 「乖離が発生してから解消されるまでの時間」の計測解像度が落ちるため。
//!
//! DEX 数 N に対して比較ペアは N(N-1)/2 通り。ペアの列挙は [`pairs`] が担当し、
//! DEX が増えても上位レイヤーのコード変更は要らない。板が片側しか無い
//! （= その銘柄がその DEX に存在しない）ペアは正常系としてスキップされる。

pub mod divergence;
pub mod pairs;
pub mod store;

pub use divergence::{DivergenceSnapshot, ExecutableDirection, DEPTH_SLIPPAGE_BPS};
pub use pairs::{dex_pairs, pairs_involving};
pub use store::BookStore;
