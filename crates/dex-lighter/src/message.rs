//! Lighter の WS メッセージのワイヤ型。
//!
//! # 板（`order_book:{MARKET_INDEX}`）
//!
//! ```json
//! {"channel":"order_book:1","last_updated_at":1700000000000,"offset":42,
//!  "order_book":{"code":0,
//!    "asks":[{"price":"36001.0","size":"2.0"}],
//!    "bids":[{"price":"36000.5","size":"1.5"}]}}
//! ```
//!
//! - 購読時に完全なスナップショットが届き、以降は差分のみ
//! - 順序検証は `begin_nonce` / `nonce` で行う
//! - **`offset` は順序検証に使わない**。API サーバーに紐づく値で、再接続で別
//!   サーバーにルーティングされると大きく変動し、連続性が保証されないため
//! - `last_updated_at` を `MessageTrace.exchange_ts_ms` に使う
//!
//! # シンボルマッピング（`market_stats:all`）
//!
//! Lighter は銘柄を文字列ではなく **market_index（数値）** で識別する。起動時に
//! `market_stats:all` を購読し、`symbol` と `market_id` からマッピングを動的に
//! 構築する（ハードコードしない）。

use rust_decimal::Decimal;
use serde::Deserialize;

/// 数値が文字列でも数値でも来る箇所を吸収する。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Num {
    Int(u64),
    Str(String),
}

impl Num {
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Num::Int(v) => Some(*v),
            Num::Str(s) => s.trim().parse::<u64>().ok(),
        }
    }
}

/// 板の 1 レベル。`{"price":..,"size":..}` と `["price","size"]` の両形式。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum PriceSize {
    Object { price: Decimal, size: Decimal },
    Tuple(Vec<Decimal>),
}

impl PriceSize {
    pub fn parts(&self) -> Option<(Decimal, Decimal)> {
        match self {
            PriceSize::Object { price, size } => Some((*price, *size)),
            PriceSize::Tuple(v) => match (v.first(), v.get(1)) {
                (Some(p), Some(s)) => Some((*p, *s)),
                _ => None,
            },
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OrderBookPayload {
    #[serde(default)]
    pub code: Option<i64>,
    #[serde(default)]
    pub asks: Vec<PriceSize>,
    #[serde(default)]
    pub bids: Vec<PriceSize>,
    /// 更新後のバージョン。順序検証に使う。
    #[serde(default)]
    pub nonce: Option<Num>,
    /// この更新が前提とするバージョン。直前の `nonce` と一致すべき。
    #[serde(default)]
    pub begin_nonce: Option<Num>,
    /// API サーバーに紐づく値。**順序検証には使わない。**
    #[serde(default)]
    pub offset: Option<Num>,
}

/// 受信メッセージの外枠。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LighterEnvelope {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub order_book: Option<OrderBookPayload>,
    #[serde(default)]
    pub market_stats: Option<serde_json::Value>,
    #[serde(default)]
    pub last_updated_at: Option<Num>,
    #[serde(default)]
    pub nonce: Option<Num>,
    #[serde(default)]
    pub begin_nonce: Option<Num>,
    #[serde(default)]
    pub offset: Option<Num>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LighterMessageKind {
    OrderBook,
    MarketStats,
    Ping,
    Pong,
    Error,
    Control,
    Unknown,
}

/// スナップショットか差分かのヒント（`type` が無い実装向けに `Option`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateKind {
    Snapshot,
    Update,
}

impl LighterEnvelope {
    pub fn message_kind(&self) -> LighterMessageKind {
        let kind = self.kind.to_ascii_lowercase();
        if kind.contains("ping") {
            return LighterMessageKind::Ping;
        }
        if kind.contains("pong") {
            return LighterMessageKind::Pong;
        }
        if kind.contains("error") {
            return LighterMessageKind::Error;
        }
        if self.order_book.is_some() {
            return LighterMessageKind::OrderBook;
        }
        if self.market_stats.is_some() {
            return LighterMessageKind::MarketStats;
        }
        if kind.contains("connected") || kind.contains("subscribed") {
            return LighterMessageKind::Control;
        }
        LighterMessageKind::Unknown
    }

    /// `type` からスナップショット/差分を判別する。判別できなければ `None`
    /// （呼び出し側がローカル板の状態で決める）。
    pub fn update_kind(&self) -> Option<UpdateKind> {
        let kind = self.kind.to_ascii_lowercase();
        if kind.contains("subscribed") || kind.contains("snapshot") {
            Some(UpdateKind::Snapshot)
        } else if kind.contains("update") {
            Some(UpdateKind::Update)
        } else {
            None
        }
    }

    /// 取引所側タイムスタンプ（ms epoch）。
    pub fn exchange_ts_ms(&self) -> Option<u64> {
        self.last_updated_at.as_ref().and_then(Num::as_u64)
    }

    /// このメッセージの `nonce`（外枠優先、無ければ order_book 内）。
    pub fn nonce(&self) -> Option<u64> {
        self.nonce
            .as_ref()
            .and_then(Num::as_u64)
            .or_else(|| self.order_book.as_ref()?.nonce.as_ref()?.as_u64())
    }

    /// このメッセージの `begin_nonce`。
    pub fn begin_nonce(&self) -> Option<u64> {
        self.begin_nonce
            .as_ref()
            .and_then(Num::as_u64)
            .or_else(|| self.order_book.as_ref()?.begin_nonce.as_ref()?.as_u64())
    }

    /// `order_book:5` / `order_book/5` → `5`
    pub fn market_index(&self) -> Option<u32> {
        channel_index(self.channel.as_deref()?)
    }
}

/// チャンネル名から market_index を取り出す。
pub fn channel_index(channel: &str) -> Option<u32> {
    channel
        .rsplit([':', '/'])
        .next()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

/// `market_stats` ペイロードから `(symbol, market_id)` を拾う。
///
/// 単一オブジェクト・配列・`{"0": {...}, "1": {...}}` のようなマップ、いずれの
/// 形でも拾えるよう再帰的に探す。
pub fn collect_market_stats(value: &serde_json::Value) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    walk_market_stats(value, 0, &mut out);
    out
}

fn walk_market_stats(value: &serde_json::Value, depth: usize, out: &mut Vec<(String, u32)>) {
    if depth > 8 {
        return;
    }
    match value {
        serde_json::Value::Object(map) => {
            let symbol = map.get("symbol").and_then(|v| v.as_str());
            let market_id = map.get("market_id").and_then(as_u32);
            if let (Some(symbol), Some(market_id)) = (symbol, market_id) {
                out.push((symbol.to_string(), market_id));
            }
            for v in map.values() {
                walk_market_stats(v, depth + 1, out);
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                walk_market_stats(v, depth + 1, out);
            }
        }
        _ => {}
    }
}

fn as_u32(value: &serde_json::Value) -> Option<u32> {
    match value {
        serde_json::Value::Number(n) => n.as_u64().and_then(|v| u32::try_from(v).ok()),
        serde_json::Value::String(s) => s.trim().parse::<u32>().ok(),
        _ => None,
    }
}

/// 全市場の統計チャンネル（シンボル → market_index の解決に使う）。
pub const MARKET_STATS_CHANNEL: &str = "market_stats:all";

pub fn order_book_channel(market_index: u32) -> String {
    format!("order_book:{market_index}")
}

pub fn subscribe_message(channel: &str) -> String {
    format!(r#"{{"type":"subscribe","channel":"{channel}"}}"#)
}

pub fn unsubscribe_message(channel: &str) -> String {
    format!(r#"{{"type":"unsubscribe","channel":"{channel}"}}"#)
}

/// クライアント側 keepalive。2 分間フレームを送らないと切断される。
pub fn ping_message() -> String {
    r#"{"type":"ping"}"#.to_string()
}

pub fn pong_message() -> String {
    r#"{"type":"pong"}"#.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    const SNAPSHOT: &str = r#"{
        "type": "subscribed/order_book",
        "channel": "order_book:1",
        "last_updated_at": 1700000000000,
        "offset": 42,
        "order_book": {
            "code": 0,
            "nonce": 100,
            "asks": [{"price": "36001.0", "size": "2.0"}, {"price": "36002.0", "size": "1.0"}],
            "bids": [{"price": "36000.5", "size": "1.5"}, {"price": "36000.0", "size": "3.0"}]
        }
    }"#;

    #[test]
    fn parses_order_book_snapshot() {
        let env: LighterEnvelope = serde_json::from_str(SNAPSHOT).unwrap();
        assert_eq!(env.message_kind(), LighterMessageKind::OrderBook);
        assert_eq!(env.update_kind(), Some(UpdateKind::Snapshot));
        assert_eq!(env.market_index(), Some(1));
        assert_eq!(env.exchange_ts_ms(), Some(1700000000000));
        assert_eq!(env.nonce(), Some(100));

        let ob = env.order_book.unwrap();
        assert_eq!(ob.bids[0].parts(), Some((dec!(36000.5), dec!(1.5))));
        assert_eq!(ob.asks.len(), 2);
    }

    #[test]
    fn parses_order_book_update_with_nonces() {
        let raw = r#"{"type":"update/order_book","channel":"order_book:1",
            "last_updated_at":"1700000000050",
            "order_book":{"code":0,"begin_nonce":100,"nonce":101,"offset":"9999",
              "bids":[{"price":"36000.75","size":"0.5"}],"asks":[]}}"#;
        let env: LighterEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(env.update_kind(), Some(UpdateKind::Update));
        assert_eq!(env.begin_nonce(), Some(100));
        assert_eq!(env.nonce(), Some(101));
        // 文字列のタイムスタンプも読める
        assert_eq!(env.exchange_ts_ms(), Some(1700000000050));
    }

    #[test]
    fn nonce_can_live_at_top_level() {
        let raw = r#"{"channel":"order_book:2","begin_nonce":7,"nonce":8,
            "order_book":{"bids":[],"asks":[]}}"#;
        let env: LighterEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(env.begin_nonce(), Some(7));
        assert_eq!(env.nonce(), Some(8));
        // type が無い場合は判別しない（呼び出し側がローカル板の状態で決める）
        assert_eq!(env.update_kind(), None);
    }

    #[test]
    fn extracts_market_index_from_channel() {
        assert_eq!(channel_index("order_book:0"), Some(0));
        assert_eq!(channel_index("order_book:15"), Some(15));
        assert_eq!(channel_index("order_book/15"), Some(15));
        assert_eq!(channel_index("market_stats:all"), None);
    }

    #[test]
    fn collects_market_stats_mapping() {
        let raw = r#"{"channel":"market_stats:0","market_stats":{
            "symbol":"ETH","market_id":0,"index_price":"2000.0","mark_price":"2000.5",
            "current_funding_rate":"0.0001","funding_rate":"0.0001"}}"#;
        let env: LighterEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(env.message_kind(), LighterMessageKind::MarketStats);

        let stats = collect_market_stats(&env.market_stats.unwrap());
        assert_eq!(stats, vec![("ETH".to_string(), 0)]);
    }

    #[test]
    fn collects_market_stats_from_map_form() {
        let raw = r#"{"channel":"market_stats:all","market_stats":{
            "0":{"symbol":"ETH","market_id":0},
            "1":{"symbol":"BTC","market_id":"1"},
            "24":{"symbol":"HYPE","market_id":24}}}"#;
        let env: LighterEnvelope = serde_json::from_str(raw).unwrap();
        let mut stats = collect_market_stats(&env.market_stats.unwrap());
        stats.sort();
        assert_eq!(
            stats,
            vec![
                ("BTC".to_string(), 1),
                ("ETH".to_string(), 0),
                ("HYPE".to_string(), 24),
            ]
        );
    }

    #[test]
    fn classifies_control_messages() {
        let ping: LighterEnvelope = serde_json::from_str(r#"{"type":"ping"}"#).unwrap();
        assert_eq!(ping.message_kind(), LighterMessageKind::Ping);

        let pong: LighterEnvelope = serde_json::from_str(r#"{"type":"pong"}"#).unwrap();
        assert_eq!(pong.message_kind(), LighterMessageKind::Pong);

        let connected: LighterEnvelope = serde_json::from_str(r#"{"type":"connected"}"#).unwrap();
        assert_eq!(connected.message_kind(), LighterMessageKind::Control);

        let err: LighterEnvelope =
            serde_json::from_str(r#"{"type":"error","message":"bad channel"}"#).unwrap();
        assert_eq!(err.message_kind(), LighterMessageKind::Error);

        let unknown: LighterEnvelope = serde_json::from_str(r#"{"type":"whatever"}"#).unwrap();
        assert_eq!(unknown.message_kind(), LighterMessageKind::Unknown);
    }

    #[test]
    fn builds_channels_and_control_messages() {
        assert_eq!(order_book_channel(24), "order_book:24");
        assert_eq!(MARKET_STATS_CHANNEL, "market_stats:all");
        assert_eq!(
            subscribe_message("order_book:24"),
            r#"{"type":"subscribe","channel":"order_book:24"}"#
        );
        assert_eq!(
            unsubscribe_message("order_book:24"),
            r#"{"type":"unsubscribe","channel":"order_book:24"}"#
        );
        assert_eq!(ping_message(), r#"{"type":"ping"}"#);
    }
}
