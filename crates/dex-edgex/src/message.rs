//! edgeX WS メッセージのワイヤ型とパース。
//!
//! # 前提と注意
//!
//! edgeX の WS 仕様はフェーズ1 設計書の「未確定事項」に挙がっているものであり、
//! ここでの型は公開ドキュメントに基づく**想定形式**である。実 API と突き合わせて
//! 差異があればこのモジュールと [`crate::book_builder`] のテストを更新すること。
//! パーサは想定外のフィールドを無視し、価格レベルは配列形式・オブジェクト形式の
//! どちらでも受け付けるようにしてある。
//!
//! 購読メッセージ:
//! ```json
//! {"type":"subscribe","channel":"depth.10000001.15"}
//! ```
//!
//! 受信メッセージ:
//! ```json
//! {"type":"quote-event","channel":"depth.10000001.15","ts":1700000000123,
//!  "content":{"dataType":"Snapshot","data":[{"contractId":"10000001",
//!    "startVersion":"100","endVersion":"101",
//!    "asks":[["36001.0","2.0"]],"bids":[["36000.5","1.5"]]}]}}
//! ```
//!
//! Hyperliquid と違い**差分更新（`dataType: "Changed"`）**が届くため、ローカルで
//! 板を再構築する必要がある（[`crate::book_builder::BookBuilder`]）。

use rust_decimal::Decimal;
use serde::Deserialize;

/// 受信メッセージの外枠。
#[derive(Debug, Deserialize)]
pub struct EdgeXEnvelope {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub content: Option<QuoteContent>,
    /// 取引所側のタイムスタンプ（ms epoch）。文字列で返る場合もある。
    #[serde(default)]
    pub ts: Option<Num>,
    #[serde(default)]
    pub time: Option<Num>,
}

impl EdgeXEnvelope {
    /// 取引所タイムスタンプ（ms epoch）。`ts` を優先し、無ければ `time`。
    pub fn exchange_ts_ms(&self) -> Option<u64> {
        self.ts
            .as_ref()
            .and_then(Num::as_u64)
            .or_else(|| self.time.as_ref().and_then(Num::as_u64))
    }

    pub fn message_kind(&self) -> EdgeXMessageKind {
        match self.kind.to_ascii_lowercase().as_str() {
            "ping" => EdgeXMessageKind::Ping,
            "pong" => EdgeXMessageKind::Pong,
            "error" => EdgeXMessageKind::Error,
            "subscribed" | "unsubscribed" | "connected" => EdgeXMessageKind::Control,
            "quote-event" | "payload" | "snapshot" | "push" => {
                if self.depth().is_some() {
                    EdgeXMessageKind::Depth
                } else {
                    EdgeXMessageKind::Control
                }
            }
            _ => {
                if self.depth().is_some() {
                    EdgeXMessageKind::Depth
                } else {
                    EdgeXMessageKind::Unknown
                }
            }
        }
    }

    /// 最初の depth ペイロード。depth 以外のチャネルなら `None`。
    pub fn depth(&self) -> Option<&DepthData> {
        let content = self.content.as_ref()?;
        let data = content.data.first()?;
        // depth チャネル以外（ticker 等）を誤って板として扱わないための最低限の判定
        if data.asks.is_empty() && data.bids.is_empty() && data.contract_id.is_none() {
            return None;
        }
        Some(data)
    }

    /// スナップショットか差分か。
    pub fn data_type(&self) -> DataType {
        self.content
            .as_ref()
            .and_then(|c| c.data_type.as_deref())
            .map(DataType::parse)
            .unwrap_or(DataType::Unknown)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeXMessageKind {
    Depth,
    Ping,
    Pong,
    Control,
    Error,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    /// 全量スナップショット。ローカルの板を置き換える。
    Snapshot,
    /// 差分。ローカルの板に適用する。
    Changed,
    Unknown,
}

impl DataType {
    fn parse(raw: &str) -> DataType {
        let lower = raw.to_ascii_lowercase();
        if lower.contains("snapshot") || lower == "all" {
            DataType::Snapshot
        } else if lower.contains("chang") || lower.contains("delta") || lower.contains("update") {
            DataType::Changed
        } else {
            DataType::Unknown
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct QuoteContent {
    #[serde(rename = "dataType", default)]
    pub data_type: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub data: Vec<DepthData>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DepthData {
    #[serde(default)]
    pub contract_id: Option<String>,
    #[serde(default)]
    pub contract_name: Option<String>,
    /// この差分が前提とするバージョン。直前の `end_version` と一致しなければ欠損。
    #[serde(default)]
    pub start_version: Option<Num>,
    /// この更新適用後のバージョン。
    #[serde(default)]
    pub end_version: Option<Num>,
    #[serde(default)]
    pub level: Option<u32>,
    #[serde(default)]
    pub asks: Vec<RawLevel>,
    #[serde(default)]
    pub bids: Vec<RawLevel>,
}

impl DepthData {
    pub fn start_version_u64(&self) -> Option<u64> {
        self.start_version.as_ref().and_then(Num::as_u64)
    }

    pub fn end_version_u64(&self) -> Option<u64> {
        self.end_version.as_ref().and_then(Num::as_u64)
    }
}

/// 価格レベル。`["36000.5","1.5"]` と `{"price":"36000.5","size":"1.5"}` の
/// どちらの形式でも受け付ける。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawLevel {
    Tuple(Vec<Decimal>),
    Object {
        price: Decimal,
        #[serde(alias = "size", alias = "quantity", alias = "amount")]
        size: Decimal,
    },
}

impl RawLevel {
    /// `(price, size)`。要素数が足りない配列は `None`。
    pub fn parts(&self) -> Option<(Decimal, Decimal)> {
        match self {
            RawLevel::Tuple(v) => match (v.first(), v.get(1)) {
                (Some(p), Some(s)) => Some((*p, *s)),
                _ => None,
            },
            RawLevel::Object { price, size } => Some((*price, *size)),
        }
    }
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

/// depth チャネル名を組み立てる（例: `depth.10000001.15`）。
pub fn depth_channel(contract_id: &str, level: usize) -> String {
    format!("depth.{contract_id}.{level}")
}

pub fn subscribe_message(channel: &str) -> String {
    format!(r#"{{"type":"subscribe","channel":"{channel}"}}"#)
}

pub fn unsubscribe_message(channel: &str) -> String {
    format!(r#"{{"type":"unsubscribe","channel":"{channel}"}}"#)
}

pub fn ping_message(now_ms: u64) -> String {
    format!(r#"{{"type":"ping","time":"{now_ms}"}}"#)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    pub const SNAPSHOT_SAMPLE: &str = r#"{
        "type": "quote-event",
        "channel": "depth.10000001.15",
        "ts": 1700000000123,
        "content": {
            "dataType": "Snapshot",
            "channel": "depth.10000001.15",
            "data": [{
                "contractId": "10000001",
                "contractName": "BTCUSD",
                "startVersion": "100",
                "endVersion": "100",
                "level": 15,
                "asks": [["36001.0", "2.0"], ["36002.5", "4.0"]],
                "bids": [["36000.5", "1.5"], ["36000.0", "3.0"]]
            }]
        }
    }"#;

    #[test]
    fn parses_snapshot_sample() {
        let env: EdgeXEnvelope = serde_json::from_str(SNAPSHOT_SAMPLE).unwrap();
        assert_eq!(env.message_kind(), EdgeXMessageKind::Depth);
        assert_eq!(env.data_type(), DataType::Snapshot);
        assert_eq!(env.exchange_ts_ms(), Some(1700000000123));

        let depth = env.depth().unwrap();
        assert_eq!(depth.contract_id.as_deref(), Some("10000001"));
        assert_eq!(depth.end_version_u64(), Some(100));
        assert_eq!(depth.bids[0].parts(), Some((dec!(36000.5), dec!(1.5))));
        assert_eq!(depth.asks.len(), 2);
    }

    #[test]
    fn parses_changed_event() {
        let raw = r#"{"type":"quote-event","channel":"depth.10000001.15","ts":"1700000000200",
            "content":{"dataType":"Changed","data":[{"contractId":"10000001",
              "startVersion":100,"endVersion":101,
              "asks":[["36001.0","0"]],"bids":[["36000.75","0.5"]]}]}}"#;
        let env: EdgeXEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(env.data_type(), DataType::Changed);
        // ts が文字列でも数値でも読める
        assert_eq!(env.exchange_ts_ms(), Some(1700000000200));
        let depth = env.depth().unwrap();
        assert_eq!(depth.start_version_u64(), Some(100));
        assert_eq!(depth.end_version_u64(), Some(101));
        assert_eq!(depth.asks[0].parts(), Some((dec!(36001.0), dec!(0))));
    }

    #[test]
    fn accepts_object_style_levels() {
        let raw = r#"{"type":"quote-event","content":{"dataType":"Snapshot","data":[{
            "contractId":"10000002",
            "asks":[{"price":"2001.0","size":"1"}],
            "bids":[{"price":"2000.0","size":"2"}]}]}}"#;
        let env: EdgeXEnvelope = serde_json::from_str(raw).unwrap();
        let depth = env.depth().unwrap();
        assert_eq!(depth.asks[0].parts(), Some((dec!(2001.0), dec!(1))));
        assert_eq!(depth.bids[0].parts(), Some((dec!(2000.0), dec!(2))));
    }

    #[test]
    fn tolerates_extra_array_elements_and_fields() {
        let raw = r#"{"type":"quote-event","unknownField":1,"content":{"dataType":"Snapshot",
            "data":[{"contractId":"10000001","futureField":true,
              "asks":[["36001.0","2.0","7"]],"bids":[]}]}}"#;
        let env: EdgeXEnvelope = serde_json::from_str(raw).unwrap();
        let depth = env.depth().unwrap();
        assert_eq!(depth.asks[0].parts(), Some((dec!(36001.0), dec!(2.0))));
    }

    #[test]
    fn classifies_control_messages() {
        let ping: EdgeXEnvelope = serde_json::from_str(r#"{"type":"ping","time":"17"}"#).unwrap();
        assert_eq!(ping.message_kind(), EdgeXMessageKind::Ping);

        let pong: EdgeXEnvelope = serde_json::from_str(r#"{"type":"pong"}"#).unwrap();
        assert_eq!(pong.message_kind(), EdgeXMessageKind::Pong);

        let sub: EdgeXEnvelope =
            serde_json::from_str(r#"{"type":"subscribed","channel":"depth.1.15"}"#).unwrap();
        assert_eq!(sub.message_kind(), EdgeXMessageKind::Control);

        let err: EdgeXEnvelope =
            serde_json::from_str(r#"{"type":"error","content":{"msg":"bad channel"}}"#).unwrap();
        assert_eq!(err.message_kind(), EdgeXMessageKind::Error);

        // depth 以外のチャネル（ticker 等）は板として扱わない
        let ticker: EdgeXEnvelope = serde_json::from_str(
            r#"{"type":"quote-event","channel":"ticker.10000001","content":{"data":[{}]}}"#,
        )
        .unwrap();
        assert!(ticker.depth().is_none());
        assert_eq!(ticker.message_kind(), EdgeXMessageKind::Control);
    }

    #[test]
    fn data_type_parsing_is_lenient() {
        assert_eq!(DataType::parse("Snapshot"), DataType::Snapshot);
        assert_eq!(DataType::parse("SNAPSHOT"), DataType::Snapshot);
        assert_eq!(DataType::parse("Changed"), DataType::Changed);
        assert_eq!(DataType::parse("DELTA"), DataType::Changed);
        assert_eq!(DataType::parse("weird"), DataType::Unknown);
    }

    #[test]
    fn builds_channel_and_control_messages() {
        assert_eq!(depth_channel("10000001", 15), "depth.10000001.15");
        assert_eq!(
            subscribe_message("depth.10000001.15"),
            r#"{"type":"subscribe","channel":"depth.10000001.15"}"#
        );
        assert_eq!(
            unsubscribe_message("depth.1.15"),
            r#"{"type":"unsubscribe","channel":"depth.1.15"}"#
        );
        assert_eq!(ping_message(42), r#"{"type":"ping","time":"42"}"#);
    }
}
