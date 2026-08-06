//! Lighter の Market Data（板受信・差分再構築・正規化）。
//!
//! フェーズ1 では板の購読のみを扱う。発注・署名ロジックはフェーズ3 で追加する。
//!
//! # このDEXの特徴
//!
//! - **WS 一本化**。板もシンボルマッピングも WS で完結するため、フェーズ1 では
//!   REST を一切使わない（レートリミット・IP ban を考慮しなくてよい）。
//! - 銘柄は文字列ではなく **market_index（数値）** で識別する。起動時に
//!   `market_stats:all` を購読し、`symbol` と `market_id` からマッピングを
//!   動的に構築する（ハードコードしない）。
//! - 購読時に完全なスナップショットが届き、以降は差分。順序検証は
//!   `begin_nonce` / `nonce` で行い、**`offset` は使わない**。
//! - **クライアント側が 2 分に 1 回以上フレームを送る責任がある**
//!   （Aster と逆で、こちらから keepalive を送る）。
//! - 読み取りが遅れているクライアントは積極的に切断されるため、受信処理では
//!   重い仕事をしない。
//!
//! testnet が提供されているため、フェーズ3 の機能検証にも使える
//! （`use_testnet = true`）。

pub mod book_builder;
pub mod message;
pub mod ws;

pub use book_builder::{ApplyError, LighterBookBuilder};
pub use message::{
    collect_market_stats, LighterEnvelope, LighterMessageKind, UpdateKind, MARKET_STATS_CHANNEL,
};
pub use ws::LighterMarketData;
