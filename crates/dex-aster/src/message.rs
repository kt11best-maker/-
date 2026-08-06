//! Aster（Binance 系 API）の WS / REST メッセージのワイヤ型。
//!
//! # 差分イベント（`<symbol>@depth@100ms`）
//!
//! ```json
//! {"e":"depthUpdate","E":123456789,"T":123456788,"s":"BTCUSDT",
//!  "U":100,"u":120,"pu":99,
//!  "bids":[["0.0024","10"]],"asks":[["0.0026","100"]]}
//! ```
//!
//! - `U` … このイベント内の最初の update ID
//! - `u` … このイベント内の最後の update ID
//! - `pu` … 直前イベントの `u`。ここが繋がらなければパケットロス
//! - 数量は相対変化ではなく**その価格の絶対数量**。0 は削除を意味する
//!
//! Binance 系は板の配列キーに `b`/`a` を使う実装と `bids`/`asks` を使う実装が
//! あるため、どちらでも読めるようにしている。

use core_types::{Dex, Symbol};
use rust_decimal::Decimal;
use serde::Deserialize;

/// 板の 1 レベル。`["価格","数量"]` と `{"price":..,"qty":..}` の両形式を受ける。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawLevel {
    Tuple(Vec<Decimal>),
    Object {
        price: Decimal,
        #[serde(alias = "qty", alias = "size", alias = "quantity")]
        quantity: Decimal,
    },
}

impl RawLevel {
    /// `(価格, 数量)`。要素数が足りない配列は `None`。
    pub fn parts(&self) -> Option<(Decimal, Decimal)> {
        match self {
            RawLevel::Tuple(v) => match (v.first(), v.get(1)) {
                (Some(p), Some(q)) => Some((*p, *q)),
                _ => None,
            },
            RawLevel::Object { price, quantity } => Some((*price, *quantity)),
        }
    }
}

/// `depthUpdate` イベント。
#[derive(Debug, Clone, Deserialize)]
pub struct DepthEvent {
    #[serde(rename = "e", default)]
    pub event_type: String,
    /// イベント時刻（ms epoch）。`MessageTrace.exchange_ts_ms` に使う。
    #[serde(rename = "E", default)]
    pub event_time_ms: Option<u64>,
    #[serde(rename = "T", default)]
    pub transaction_time_ms: Option<u64>,
    #[serde(rename = "s", default)]
    pub symbol_raw: String,
    #[serde(rename = "U")]
    pub first_update_id: u64,
    #[serde(rename = "u")]
    pub final_update_id: u64,
    /// 直前ストリームの `u`。提供されない実装もあるため `Option`。
    #[serde(rename = "pu", default)]
    pub prev_final_update_id: Option<u64>,
    #[serde(default, alias = "b")]
    pub bids: Vec<RawLevel>,
    #[serde(default, alias = "a")]
    pub asks: Vec<RawLevel>,
}

impl DepthEvent {
    pub fn symbol(&self) -> Option<Symbol> {
        Symbol::from_dex_symbol(Dex::Aster, &self.symbol_raw)
    }
}

/// REST `/fapi/v1/depth` のレスポンス。
#[derive(Debug, Clone, Deserialize)]
pub struct DepthSnapshot {
    #[serde(rename = "lastUpdateId")]
    pub last_update_id: u64,
    #[serde(rename = "E", default)]
    pub event_time_ms: Option<u64>,
    #[serde(rename = "T", default)]
    pub transaction_time_ms: Option<u64>,
    #[serde(default, alias = "b")]
    pub bids: Vec<RawLevel>,
    #[serde(default, alias = "a")]
    pub asks: Vec<RawLevel>,
}

/// WS のテキストメッセージから `depthUpdate` を取り出す。
///
/// 単一ストリーム（`/ws/<stream>`）と結合ストリーム（`/stream?streams=...` の
/// `{"stream":..,"data":{..}}` 形式）の両方を受け付ける。depth 以外のメッセージ
/// （購読応答など）は `Ok(None)`。
pub fn parse_depth_message(text: &str) -> Result<Option<DepthEvent>, serde_json::Error> {
    let root: serde_json::Value = serde_json::from_str(text)?;

    // 結合ストリームは data フィールドに本体が入る
    let payload = match root.get("data") {
        Some(data) if data.is_object() => data,
        _ => &root,
    };

    match payload.get("e").and_then(|v| v.as_str()) {
        Some("depthUpdate") => Ok(Some(serde_json::from_value(payload.clone())?)),
        _ => Ok(None),
    }
}

/// 結合ストリームの URL を組み立てる。
pub fn combined_stream_url(base_ws_url: &str, streams: &[String]) -> String {
    format!(
        "{}/stream?streams={}",
        base_ws_url.trim_end_matches('/'),
        streams.join("/")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    /// 結合ストリーム形式の実レスポンスに合わせた固定サンプル。
    const DEPTH_EVENT: &str = r#"{
        "stream": "btcusdt@depth@100ms",
        "data": {
            "e": "depthUpdate",
            "E": 1700000000123,
            "T": 1700000000120,
            "s": "BTCUSDT",
            "U": 100,
            "u": 120,
            "pu": 99,
            "b": [["36000.5", "1.5"], ["36000.0", "0"]],
            "a": [["36001.0", "2.0"]]
        }
    }"#;

    const SNAPSHOT: &str = r#"{
        "lastUpdateId": 120,
        "E": 1700000000100,
        "T": 1700000000099,
        "bids": [["36000.5", "1.5"], ["36000.0", "2.0"]],
        "asks": [["36001.0", "2.0"], ["36002.0", "1.0"]]
    }"#;

    #[test]
    fn parses_combined_stream_event() {
        let ev = parse_depth_message(DEPTH_EVENT).unwrap().unwrap();
        assert_eq!(ev.symbol(), Some(Symbol::Btc));
        assert_eq!(ev.event_time_ms, Some(1700000000123));
        assert_eq!(ev.first_update_id, 100);
        assert_eq!(ev.final_update_id, 120);
        assert_eq!(ev.prev_final_update_id, Some(99));
        assert_eq!(ev.bids[0].parts(), Some((dec!(36000.5), dec!(1.5))));
        // 数量 0 は削除
        assert_eq!(ev.bids[1].parts(), Some((dec!(36000.0), dec!(0))));
        assert_eq!(ev.asks.len(), 1);
    }

    #[test]
    fn parses_bare_event_and_bids_asks_keys() {
        let raw = r#"{"e":"depthUpdate","E":1,"s":"ETHUSDT","U":1,"u":2,"pu":0,
            "bids":[["2000.0","1"]],"asks":[["2001.0","1"]]}"#;
        let ev = parse_depth_message(raw).unwrap().unwrap();
        assert_eq!(ev.symbol(), Some(Symbol::Eth));
        assert_eq!(ev.bids[0].parts(), Some((dec!(2000.0), dec!(1))));
    }

    #[test]
    fn ignores_non_depth_messages() {
        assert!(parse_depth_message(r#"{"result":null,"id":1}"#)
            .unwrap()
            .is_none());
        assert!(parse_depth_message(r#"{"e":"aggTrade","s":"BTCUSDT"}"#)
            .unwrap()
            .is_none());
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(parse_depth_message("not json").is_err());
    }

    #[test]
    fn parses_snapshot() {
        let snap: DepthSnapshot = serde_json::from_str(SNAPSHOT).unwrap();
        assert_eq!(snap.last_update_id, 120);
        assert_eq!(snap.bids.len(), 2);
        assert_eq!(snap.asks[0].parts(), Some((dec!(36001.0), dec!(2.0))));
    }

    #[test]
    fn unknown_symbol_is_none() {
        let raw = r#"{"e":"depthUpdate","E":1,"s":"DOGEUSDT","U":1,"u":2,"b":[],"a":[]}"#;
        let ev = parse_depth_message(raw).unwrap().unwrap();
        assert_eq!(ev.symbol(), None);
    }

    #[test]
    fn builds_combined_stream_url() {
        let url = combined_stream_url(
            "wss://fstream.asterdex.com",
            &["btcusdt@depth@100ms".into(), "ethusdt@depth@100ms".into()],
        );
        assert_eq!(
            url,
            "wss://fstream.asterdex.com/stream?streams=btcusdt@depth@100ms/ethusdt@depth@100ms"
        );
    }
}
