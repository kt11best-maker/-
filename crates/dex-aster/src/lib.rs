//! Aster の Market Data（板受信・差分再構築・正規化）。
//!
//! フェーズ1 では板の購読のみを扱う。発注・署名ロジックはフェーズ3 で追加する。
//!
//! # このDEXの特徴
//!
//! - Binance 系の**差分更新方式**。基準スナップショットは REST でしか取れないため、
//!   4 DEX の中で唯一 REST 依存が構造的に外せない。
//! - 板の初期化・再同期は `U`/`u`/`pu` の連続性検証に基づく（[`book_builder`]）。
//! - WS 接続は 24 時間で強制切断されるので、その前に能動的に張り直す。
//! - サーバーから 5 分ごとに ping frame が来る。15 分以内に pong を返さないと切断。
//! - レートリミットは IP 単位で、違反を繰り返すと最大 3 日 ban される。
//!   再同期時の REST 呼び出しは `resync_backoff_ms` で必ず間隔を空けること。
//!
//! > **注**: Aster には Spot 用の `sapi` と Futures/Perp 用の `fapi` があり、
//! > 本プロジェクトは perp が対象なので `fapi` を使う。また Pro Mode（CLOB）と
//! > Simple Mode（ALP プール）があり、アービトラージ対象は Pro Mode の CLOB のみ。

pub mod book_builder;
pub mod message;
pub mod rest;
pub mod ws;

pub use book_builder::{ApplyError, ApplyOutcome, AsterBookBuilder, BookPhase};
pub use message::{parse_depth_message, DepthEvent, DepthSnapshot};
pub use ws::AsterMarketData;
