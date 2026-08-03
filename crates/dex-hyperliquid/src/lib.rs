//! Hyperliquid の Market Data（板受信・正規化）。
//!
//! フェーズ1では板の購読のみを扱う。発注・署名ロジックはフェーズ3で追加する。

pub mod message;
pub mod ws;

pub use message::{to_order_book, HlMessage, L2BookData, SubscribeRequest};
pub use ws::HyperliquidMarketData;
