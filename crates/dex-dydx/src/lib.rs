//! dYdX v4（Indexer 経由）の Market Data クライアント。
//!
//! # dYdX 固有の注意点（乖離判定に直結）
//!
//! ## 板がクロスすることがある
//!
//! dYdX は中央集権的なオーダーブックを持たないため、**bid が ask より高い
//! （クロスした）板が観測されうる**。これは異常データではなく構造上正常に
//! 起こる現象。
//!
//! - クロスした板も**捨てずに**下流へ流す（発生頻度の計測がフェーズ1 の目的）
//! - CSV には `book_crossed_a` / `book_crossed_b` として残す
//! - **クロスした板から計算した乖離はアービトラージ機会として扱わない**
//!   （`strategy-price-arb` で除外する）
//!
//! ## Indexer のデータ鮮度
//!
//! Indexer はブロックチェーンの状態を追ってDBに反映する中間層であり、真に
//! 正しい板（ブロックプロポーザーの mempool 内）とは差がある。**dYdX の板は
//! 構造的に「少し古い」可能性がある。** `staleness_delta_ms` が dYdX を含む
//! 組み合わせで系統的に大きくなっていないか必ず確認すること。大きい場合、
//! dYdX との乖離の多くは「見かけ上の乖離」である可能性が高い。
//!
//! なお板メッセージに取引所側タイムスタンプが含まれないため、`latency_ms` は
//! 空欄になる。鮮度は `staleness_delta_ms` で見ること。

pub mod book_builder;
pub mod message;
pub mod ws;

pub use book_builder::{ApplyError, DydxBookBuilder};
pub use message::{DydxEnvelope, DydxMessageKind};
pub use ws::DydxMarketData;
