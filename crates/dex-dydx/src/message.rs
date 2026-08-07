//! dYdX v4 Indexer の WS メッセージのワイヤ型。
//!
//! # 接続フロー
//!
//! 1. WS 接続を確立する
//! 2. サーバーから `{"type":"connected", ...}` が届く
//! 3. **`connected` を受信してから**購読メッセージを送る
//!    （接続直後に送ってはいけない）
//!
//! ```json
//! {"type":"subscribe","channel":"v4_orderbook","id":"BTC-USD"}
//! ```
//!
//! # メッセージ
//!
//! - `subscribed` … 全量スナップショット。**受信時にローカル板を必ずリセット**
//!   してから作り直す
//! - `channel_data` … 差分。**`size` が 0 の価格レベルは削除**
//! - `message_id` … 接続ごとの論理オフセット。順序保証・欠損検知に使う
//!
//! # Ping/Pong（既存 DEX と仕組みが違う）
//!
//! dYdX は WS **プロトコルレベルの制御フレーム**で ping を送ってくる
//! （30 秒ごと、10 秒以内に pong を返さないと切断）。edgeX のような JSON の
//! `{"type":"ping"}` ではない。`tokio-tungstenite` の自動応答に任せず、
//! 受信ループで明示的に `Message::Pong` を返している。

use rust_decimal::Decimal;
use serde::Deserialize;

/// 板の 1 レベル。
///
/// スナップショットは `{"price":"...","size":"..."}`、差分は
/// `["price","size"]` の形で届く（実装時に確認すること）。どちらでも読める
/// ようにしてある。
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

/// `contents` の中身。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OrderBookContents {
    #[serde(default)]
    pub bids: Vec<PriceSize>,
    #[serde(default)]
    pub asks: Vec<PriceSize>,
}

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

/// 受信メッセージの外枠。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DydxEnvelope {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub channel: Option<String>,
    /// マーケット ID（`"BTC-USD"`）。
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub contents: Option<serde_json::Value>,
    /// 接続ごとの論理オフセット。ハイフン表記で来る実装もあるため両方受ける。
    #[serde(default, rename = "message_id", alias = "message-id")]
    pub message_id: Option<Num>,
    #[serde(default)]
    pub connection_id: Option<String>,
    /// `error` のときのメッセージ本文。
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DydxMessageKind {
    /// 接続確立。これを受けてから購読する。
    Connected,
    /// 購読応答（全量スナップショット付き）。
    Subscribed,
    /// 差分更新。
    ChannelData,
    Unsubscribed,
    Error,
    Unknown,
}

impl DydxEnvelope {
    pub fn message_kind(&self) -> DydxMessageKind {
        match self.kind.as_str() {
            "connected" => DydxMessageKind::Connected,
            "subscribed" => DydxMessageKind::Subscribed,
            "channel_data" => DydxMessageKind::ChannelData,
            "unsubscribed" => DydxMessageKind::Unsubscribed,
            "error" => DydxMessageKind::Error,
            _ => DydxMessageKind::Unknown,
        }
    }

    pub fn message_id(&self) -> Option<u64> {
        self.message_id.as_ref().and_then(Num::as_u64)
    }

    /// 板チャンネルのメッセージか。
    pub fn is_orderbook(&self) -> bool {
        self.channel.as_deref() == Some(ORDERBOOK_CHANNEL)
    }

    /// `contents` を板の差分/スナップショットとして解釈する。
    ///
    /// `subscribed` では `contents` が `{"bids":[],"asks":[]}` で届くが、
    /// 実装によっては `{"orderbook":{...}}` のように 1 段包まれる可能性がある
    /// ため、そのケースも拾う。
    pub fn order_book(&self) -> Option<OrderBookContents> {
        let value = self.contents.as_ref()?;
        if let Ok(contents) = serde_json::from_value::<OrderBookContents>(value.clone()) {
            if !contents.bids.is_empty() || !contents.asks.is_empty() {
                return Some(contents);
            }
            // bids/asks が空でも「空の板」という正当な内容なので、キーの
            // 有無で判断する
            if value.get("bids").is_some() || value.get("asks").is_some() {
                return Some(contents);
            }
        }
        let inner = value.get("orderbook")?;
        serde_json::from_value::<OrderBookContents>(inner.clone()).ok()
    }
}

pub const ORDERBOOK_CHANNEL: &str = "v4_orderbook";

pub fn subscribe_message(market: &str) -> String {
    format!(r#"{{"type":"subscribe","channel":"{ORDERBOOK_CHANNEL}","id":"{market}"}}"#)
}

pub fn unsubscribe_message(market: &str) -> String {
    format!(r#"{{"type":"unsubscribe","channel":"{ORDERBOOK_CHANNEL}","id":"{market}"}}"#)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    const CONNECTED: &str = r#"{"type":"connected","connection_id":"abc","message_id":0}"#;

    const SUBSCRIBED: &str = r#"{
        "type": "subscribed",
        "connection_id": "abc",
        "message_id": 1,
        "channel": "v4_orderbook",
        "id": "BTC-USD",
        "contents": {
            "bids": [{"price": "36000.5", "size": "1.5"}, {"price": "36000.0", "size": "3.0"}],
            "asks": [{"price": "36001.0", "size": "2.0"}, {"price": "36002.0", "size": "1.0"}]
        }
    }"#;

    const CHANNEL_DATA: &str = r#"{
        "type": "channel_data",
        "connection_id": "abc",
        "message_id": 2,
        "channel": "v4_orderbook",
        "id": "BTC-USD",
        "contents": {"bids": [["36000.75", "0.5"], ["36000.0", "0"]], "asks": []}
    }"#;

    fn parse(raw: &str) -> DydxEnvelope {
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn classifies_message_kinds() {
        assert_eq!(parse(CONNECTED).message_kind(), DydxMessageKind::Connected);
        assert_eq!(
            parse(SUBSCRIBED).message_kind(),
            DydxMessageKind::Subscribed
        );
        assert_eq!(
            parse(CHANNEL_DATA).message_kind(),
            DydxMessageKind::ChannelData
        );
        assert_eq!(
            parse(r#"{"type":"unsubscribed","channel":"v4_orderbook","id":"BTC-USD"}"#)
                .message_kind(),
            DydxMessageKind::Unsubscribed
        );
        assert_eq!(
            parse(r#"{"type":"error","message":"Invalid channel"}"#).message_kind(),
            DydxMessageKind::Error
        );
        assert_eq!(
            parse(r#"{"type":"whatever"}"#).message_kind(),
            DydxMessageKind::Unknown
        );
    }

    #[test]
    fn parses_snapshot_contents() {
        let env = parse(SUBSCRIBED);
        assert!(env.is_orderbook());
        assert_eq!(env.id.as_deref(), Some("BTC-USD"));
        assert_eq!(env.message_id(), Some(1));

        let ob = env.order_book().unwrap();
        assert_eq!(ob.bids[0].parts(), Some((dec!(36000.5), dec!(1.5))));
        assert_eq!(ob.asks.len(), 2);
    }

    #[test]
    fn parses_tuple_form_updates() {
        let env = parse(CHANNEL_DATA);
        let ob = env.order_book().unwrap();
        assert_eq!(ob.bids[0].parts(), Some((dec!(36000.75), dec!(0.5))));
        // size 0 は削除を意味する。パーサーの時点では落とさずそのまま渡す
        assert_eq!(ob.bids[1].parts(), Some((dec!(36000.0), Decimal::ZERO)));
        assert!(ob.asks.is_empty());
    }

    #[test]
    fn empty_side_is_valid_content() {
        // 片側だけの更新でも「空の板」として解釈できる
        let env = parse(
            r#"{"type":"channel_data","channel":"v4_orderbook","id":"ETH-USD",
                "contents":{"asks":[["2001.0","1.0"]]}}"#,
        );
        let ob = env.order_book().unwrap();
        assert!(ob.bids.is_empty());
        assert_eq!(ob.asks.len(), 1);
    }

    #[test]
    fn unwraps_nested_orderbook_form() {
        let env = parse(
            r#"{"type":"subscribed","channel":"v4_orderbook","id":"SOL-USD",
                "contents":{"orderbook":{"bids":[["100.0","1.0"]],"asks":[["101.0","2.0"]]}}}"#,
        );
        let ob = env.order_book().unwrap();
        assert_eq!(ob.bids[0].parts(), Some((dec!(100.0), dec!(1.0))));
    }

    #[test]
    fn message_id_reads_both_spellings() {
        assert_eq!(parse(r#"{"message_id":42}"#).message_id(), Some(42));
        assert_eq!(parse(r#"{"message-id":"43"}"#).message_id(), Some(43));
        assert_eq!(parse(r#"{"type":"connected"}"#).message_id(), None);
    }

    #[test]
    fn builds_subscription_messages() {
        assert_eq!(
            subscribe_message("BTC-USD"),
            r#"{"type":"subscribe","channel":"v4_orderbook","id":"BTC-USD"}"#
        );
        assert_eq!(
            unsubscribe_message("BTC-USD"),
            r#"{"type":"unsubscribe","channel":"v4_orderbook","id":"BTC-USD"}"#
        );
    }

    #[test]
    fn non_orderbook_channels_are_recognized() {
        let env = parse(r#"{"type":"channel_data","channel":"v4_trades","id":"BTC-USD"}"#);
        assert!(!env.is_orderbook());
    }
}
