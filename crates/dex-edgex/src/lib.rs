//! edgeX の Market Data（板受信・差分再構築・正規化）。
//!
//! フェーズ1では板の購読のみを扱う。発注・署名ロジックはフェーズ3で追加する。

pub mod book_builder;
pub mod message;
pub mod meta;
pub mod ws;

pub use book_builder::{ApplyError, BookBuilder, SnapshotCheck};
pub use message::{DataType, DepthData, EdgeXEnvelope, EdgeXMessageKind};
pub use ws::EdgeXMarketData;
