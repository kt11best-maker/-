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

/// `market_stats` の 1 市場分。
///
/// シンボルマッピング（`symbol` / `market_id`）とファンディングレートが同じ
/// メッセージに入っているため、**板の購読に使っている接続からそのまま
/// ファンディングも取れる**（追加の接続は不要）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MarketStatsEntry {
    pub symbol: String,
    pub market_id: u32,
    pub index_price: Option<Decimal>,
    pub mark_price: Option<Decimal>,
    /// 現在のファンディングレート。
    pub current_funding_rate: Option<Decimal>,
    /// もう 1 つのレートフィールド。予測値として扱う。
    ///
    /// > 実装時に一次情報で意味を確認すること（予測値か直近確定値かで
    /// > `predicted_rate` への割り当てを変える）。
    pub funding_rate: Option<Decimal>,
    /// 次回精算時刻（ms epoch）。API が返す場合のみ。
    pub next_funding_time_ms: Option<u64>,
    /// 未決済建玉（**契約数量**）。
    pub open_interest: Option<Decimal>,
    /// 直近 24 時間の取引量（**クオート = USD 建て**）。
    pub daily_quote_volume: Option<Decimal>,
    /// 直近 24 時間の取引量（**ベース = 契約数量建て**）。
    ///
    /// クオート建てが無い場合に、価格を掛けて USD 換算するために使う。
    pub daily_base_volume: Option<Decimal>,
}

impl MarketStatsEntry {
    /// ファンディング関連のフィールドが 1 つでも入っているか。
    pub fn has_funding(&self) -> bool {
        self.current_funding_rate.is_some() || self.funding_rate.is_some()
    }

    /// 24 時間取引量を **USD 建て**で返す。
    ///
    /// クオート建てがあればそれをそのまま使い、無ければベース建て × 価格で
    /// 換算する。**価格が無ければ換算しない**（推測で埋めない）。
    pub fn volume_24h_usd(&self) -> Option<Decimal> {
        if let Some(quote) = self.daily_quote_volume {
            return Some(quote);
        }
        let base = self.daily_base_volume?;
        let price = self.mark_price.or(self.index_price)?;
        (price > Decimal::ZERO).then(|| base * price)
    }
}

/// `market_stats` ペイロードから市場ごとの統計を拾う。
///
/// 単一オブジェクト・配列・`{"0": {...}, "1": {...}}` のようなマップ、いずれの
/// 形でも拾えるよう再帰的に探す。
pub fn collect_market_stats(value: &serde_json::Value) -> Vec<MarketStatsEntry> {
    let mut out = Vec::new();
    walk_market_stats(value, 0, &mut out);
    out
}

fn walk_market_stats(value: &serde_json::Value, depth: usize, out: &mut Vec<MarketStatsEntry>) {
    if depth > 8 {
        return;
    }
    match value {
        serde_json::Value::Object(map) => {
            let symbol = map.get("symbol").and_then(|v| v.as_str());
            let market_id = map.get("market_id").and_then(as_u32);
            if let (Some(symbol), Some(market_id)) = (symbol, market_id) {
                out.push(MarketStatsEntry {
                    symbol: symbol.to_string(),
                    market_id,
                    index_price: map.get("index_price").and_then(as_decimal),
                    mark_price: map.get("mark_price").and_then(as_decimal),
                    current_funding_rate: map.get("current_funding_rate").and_then(as_decimal),
                    funding_rate: map.get("funding_rate").and_then(as_decimal),
                    next_funding_time_ms: map
                        .get("next_funding_time")
                        .or_else(|| map.get("next_funding_timestamp"))
                        .and_then(as_u64),
                    // 流動性指標もファンディングと同じメッセージに入っている。
                    // キー名は実 API と突き合わせて確認すること（複数の綴りを
                    // 受け付けるようにしてあるが、無ければ空欄のまま残す）。
                    open_interest: map
                        .get("open_interest")
                        .or_else(|| map.get("open_interest_base"))
                        .and_then(as_decimal),
                    daily_quote_volume: map
                        .get("daily_quote_token_volume")
                        .or_else(|| map.get("daily_quote_volume"))
                        .and_then(as_decimal),
                    daily_base_volume: map
                        .get("daily_base_token_volume")
                        .or_else(|| map.get("daily_base_volume"))
                        .and_then(as_decimal),
                });
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

fn as_u64(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

/// 価格・レートは文字列で来る。精度を落とさないよう `Decimal` で受ける。
fn as_decimal(value: &serde_json::Value) -> Option<Decimal> {
    match value {
        serde_json::Value::String(s) => s.trim().parse::<Decimal>().ok(),
        serde_json::Value::Number(n) => n.to_string().parse::<Decimal>().ok(),
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
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].symbol, "ETH");
        assert_eq!(stats[0].market_id, 0);
    }

    #[test]
    fn collects_funding_fields_from_market_stats() {
        // 板の購読と同じ接続に流れてくるメッセージからファンディングも取れる
        let raw = r#"{"channel":"market_stats:0","market_stats":{
            "symbol":"ETH","market_id":0,"index_price":"2000.0","mark_price":"2000.5",
            "current_funding_rate":"0.0000125","funding_rate":"0.0000130",
            "next_funding_time":1700000003600}}"#;
        let env: LighterEnvelope = serde_json::from_str(raw).unwrap();
        let stats = collect_market_stats(&env.market_stats.unwrap());

        let e = &stats[0];
        assert!(e.has_funding());
        assert_eq!(e.index_price, Some(dec!(2000.0)));
        assert_eq!(e.mark_price, Some(dec!(2000.5)));
        assert_eq!(e.current_funding_rate, Some(dec!(0.0000125)));
        assert_eq!(e.funding_rate, Some(dec!(0.0000130)));
        assert_eq!(e.next_funding_time_ms, Some(1700000003600));
    }

    #[test]
    fn collects_liquidity_metrics_from_market_stats() {
        // OI と出来高もファンディングと同じメッセージに入っている
        let raw = r#"{"channel":"market_stats:0","market_stats":{
            "symbol":"BTC","market_id":1,"mark_price":"36000.5",
            "current_funding_rate":"0.0000125","open_interest":"1234.5",
            "daily_quote_token_volume":"987654321.0",
            "daily_base_token_volume":"27000.0"}}"#;
        let env: LighterEnvelope = serde_json::from_str(raw).unwrap();
        let stats = collect_market_stats(&env.market_stats.unwrap());
        let e = &stats[0];

        assert_eq!(e.open_interest, Some(dec!(1234.5)));
        // クオート建てがあればそのまま USD として使う
        assert_eq!(e.volume_24h_usd(), Some(dec!(987654321.0)));
    }

    #[test]
    fn base_volume_is_converted_with_price_only() {
        let raw = r#"{"channel":"market_stats:0","market_stats":{
            "symbol":"BTC","market_id":1,"mark_price":"36000",
            "daily_base_token_volume":"10"}}"#;
        let env: LighterEnvelope = serde_json::from_str(raw).unwrap();
        let e = &collect_market_stats(&env.market_stats.unwrap())[0];
        assert_eq!(e.volume_24h_usd(), Some(dec!(360000)));

        // 価格が無ければ換算しない（数量を USD として記録してしまわない）
        let raw = r#"{"channel":"market_stats:0","market_stats":{
            "symbol":"BTC","market_id":1,"daily_base_token_volume":"10"}}"#;
        let env: LighterEnvelope = serde_json::from_str(raw).unwrap();
        let e = &collect_market_stats(&env.market_stats.unwrap())[0];
        assert_eq!(e.volume_24h_usd(), None);
    }

    #[test]
    fn market_stats_without_funding_is_still_usable_for_mapping() {
        let raw = r#"{"channel":"market_stats:all","market_stats":{
            "0":{"symbol":"ETH","market_id":0}}}"#;
        let env: LighterEnvelope = serde_json::from_str(raw).unwrap();
        let stats = collect_market_stats(&env.market_stats.unwrap());
        assert!(!stats[0].has_funding());
        assert_eq!(stats[0].market_id, 0);
    }

    #[test]
    fn collects_market_stats_from_map_form() {
        let raw = r#"{"channel":"market_stats:all","market_stats":{
            "0":{"symbol":"ETH","market_id":0},
            "1":{"symbol":"BTC","market_id":"1"},
            "24":{"symbol":"HYPE","market_id":24}}}"#;
        let env: LighterEnvelope = serde_json::from_str(raw).unwrap();
        let mut stats: Vec<(String, u32)> = collect_market_stats(&env.market_stats.unwrap())
            .into_iter()
            .map(|e| (e.symbol, e.market_id))
            .collect();
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
    fn numeric_rates_are_read_without_precision_loss() {
        // 数値形式で来ても Decimal として読める
        let raw = r#"{"channel":"market_stats:0","market_stats":{
            "symbol":"BTC","market_id":1,"current_funding_rate":0.0000125}}"#;
        let env: LighterEnvelope = serde_json::from_str(raw).unwrap();
        let stats = collect_market_stats(&env.market_stats.unwrap());
        assert_eq!(stats[0].current_funding_rate, Some(dec!(0.0000125)));
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
