//! Hyperliquid WS メッセージのワイヤ型とパース。
//!
//! 購読メッセージ:
//! ```json
//! {"method":"subscribe","subscription":{"type":"l2Book","coin":"BTC"}}
//! ```
//!
//! 受信メッセージ（`l2Book`）:
//! ```json
//! {"channel":"l2Book","data":{"coin":"BTC","time":1700000000000,
//!  "levels":[[{"px":"36000.5","sz":"1.5","n":3}],[{"px":"36001.0","sz":"2.0","n":2}]]}}
//! ```
//!
//! `levels[0]` が bid、`levels[1]` が ask。**毎回フルスナップショット**が届くため、
//! 差分再構築もシーケンス番号の欠損検知も不要（API 側にシーケンス番号自体が無い）。

use core_types::{Dex, Level, MessageTrace, OrderBook, Price, Quantity, Symbol};
use dex_traits::MarketDataError;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// 受信メッセージ。未知の channel は [`HlMessage::Other`] に落ちる。
#[derive(Debug, Deserialize)]
#[serde(tag = "channel")]
pub enum HlMessage {
    #[serde(rename = "l2Book")]
    L2Book { data: L2BookData },
    #[serde(rename = "pong")]
    Pong,
    #[serde(rename = "subscriptionResponse")]
    SubscriptionResponse {
        #[serde(default)]
        data: serde_json::Value,
    },
    #[serde(rename = "error")]
    Error { data: serde_json::Value },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct L2BookData {
    pub coin: String,
    /// 取引所側の生成時刻（ms epoch）。
    #[serde(default)]
    pub time: Option<u64>,
    /// `[bids, asks]`。
    pub levels: Vec<Vec<HlLevel>>,
}

#[derive(Debug, Deserialize)]
pub struct HlLevel {
    /// 価格。API は文字列で返す（精度を落とさないため Decimal で受ける）。
    pub px: Decimal,
    /// 数量。
    pub sz: Decimal,
    /// そのレベルの注文数。フェーズ1では未使用。
    #[serde(default)]
    pub n: u64,
}

/// 購読リクエスト。
#[derive(Debug, Serialize)]
pub struct SubscribeRequest {
    pub method: &'static str,
    pub subscription: Subscription,
}

#[derive(Debug, Serialize)]
pub struct Subscription {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub coin: &'static str,
}

impl SubscribeRequest {
    pub fn l2_book(symbol: Symbol) -> Self {
        SubscribeRequest {
            method: "subscribe",
            subscription: Subscription {
                kind: "l2Book",
                coin: symbol.to_dex_symbol(Dex::Hyperliquid),
            },
        }
    }
}

/// keepalive の ping。無通信が続くと取引所側から切断されるため定期送信する。
pub fn ping_message() -> String {
    r#"{"method":"ping"}"#.to_string()
}

/// `l2Book` データを正規化済み [`OrderBook`] に変換する。
///
/// `trace` は**受信直後に生成されたもの**を渡すこと。この関数内で
/// `exchange_ts_ms` と `normalized_instant` を埋める。
pub fn to_order_book(
    data: &L2BookData,
    depth: usize,
    mut trace: MessageTrace,
) -> Result<OrderBook, MarketDataError> {
    let symbol = Symbol::from_dex_symbol(Dex::Hyperliquid, &data.coin)
        .ok_or_else(|| MarketDataError::Parse(format!("未購読/未知の coin: {}", data.coin)))?;
    if data.levels.len() < 2 {
        return Err(MarketDataError::Parse(format!(
            "levels の要素数が不足: {}",
            data.levels.len()
        )));
    }

    trace.set_exchange_ts_ms(data.time);

    let mut bids = convert(&data.levels[0]);
    let mut asks = convert(&data.levels[1]);
    // API 側の順序を信用せず自前でソートする（正規化の契約を守るため）。
    bids.sort_by(|a, b| b.price.cmp(&a.price));
    asks.sort_by(|a, b| a.price.cmp(&b.price));
    bids.truncate(depth);
    asks.truncate(depth);

    trace.mark_normalized();
    Ok(OrderBook::new(Dex::Hyperliquid, symbol, bids, asks, trace))
}

fn convert(levels: &[HlLevel]) -> Vec<Level> {
    levels
        .iter()
        .filter(|l| l.sz > Decimal::ZERO)
        .map(|l| Level::new(Price(l.px), Quantity(l.sz)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    /// 実際の WS レスポンス形式に合わせた固定サンプル。
    const L2_BOOK_SAMPLE: &str = r#"{
        "channel": "l2Book",
        "data": {
            "coin": "BTC",
            "time": 1700000000123,
            "levels": [
                [
                    {"px": "36000.5", "sz": "1.5", "n": 3},
                    {"px": "36000.0", "sz": "2.25", "n": 5},
                    {"px": "35999.0", "sz": "0", "n": 0}
                ],
                [
                    {"px": "36001.0", "sz": "2.0", "n": 2},
                    {"px": "36002.5", "sz": "4.125", "n": 7}
                ]
            ]
        }
    }"#;

    fn parse_sample(depth: usize) -> OrderBook {
        let msg: HlMessage = serde_json::from_str(L2_BOOK_SAMPLE).unwrap();
        let HlMessage::L2Book { data } = msg else {
            panic!("l2Book として解釈されなかった");
        };
        to_order_book(&data, depth, MessageTrace::on_receive()).unwrap()
    }

    #[test]
    fn parses_l2_book_sample() {
        let book = parse_sample(20);
        assert_eq!(book.dex, Dex::Hyperliquid);
        assert_eq!(book.symbol, Symbol::Btc);
        assert_eq!(book.best_bid(), Some(Price(dec!(36000.5))));
        assert_eq!(book.best_ask(), Some(Price(dec!(36001.0))));
        assert_eq!(book.mid(), Some(Price(dec!(36000.75))));
        assert_eq!(book.trace.exchange_ts_ms, Some(1700000000123));
        assert!(book.trace.normalize_latency().is_some());
        book.validate().unwrap();
    }

    #[test]
    fn drops_zero_size_levels() {
        let book = parse_sample(20);
        // sz=0 の 35999.0 は除外される
        assert_eq!(book.bids.len(), 2);
        assert_eq!(book.asks.len(), 2);
        assert_eq!(book.bids[1].quantity, Quantity(dec!(2.25)));
    }

    #[test]
    fn truncates_to_requested_depth() {
        let book = parse_sample(1);
        assert_eq!(book.bids.len(), 1);
        assert_eq!(book.asks.len(), 1);
        assert_eq!(book.bids[0].price, Price(dec!(36000.5)));
    }

    #[test]
    fn sorts_defensively_when_exchange_order_is_odd() {
        let raw = r#"{"channel":"l2Book","data":{"coin":"ETH","time":1,"levels":[
            [{"px":"1999.0","sz":"1","n":1},{"px":"2000.0","sz":"1","n":1}],
            [{"px":"2002.0","sz":"1","n":1},{"px":"2001.0","sz":"1","n":1}]]}}"#;
        let HlMessage::L2Book { data } = serde_json::from_str(raw).unwrap() else {
            panic!()
        };
        let book = to_order_book(&data, 10, MessageTrace::on_receive()).unwrap();
        assert_eq!(book.best_bid(), Some(Price(dec!(2000))));
        assert_eq!(book.best_ask(), Some(Price(dec!(2001))));
        book.validate().unwrap();
    }

    #[test]
    fn rejects_unknown_coin() {
        let raw = r#"{"channel":"l2Book","data":{"coin":"DOGE","time":1,"levels":[[],[]]}}"#;
        let HlMessage::L2Book { data } = serde_json::from_str(raw).unwrap() else {
            panic!()
        };
        assert!(to_order_book(&data, 10, MessageTrace::on_receive()).is_err());
    }

    #[test]
    fn rejects_malformed_levels() {
        let raw = r#"{"channel":"l2Book","data":{"coin":"SOL","time":1,"levels":[[]]}}"#;
        let HlMessage::L2Book { data } = serde_json::from_str(raw).unwrap() else {
            panic!()
        };
        assert!(to_order_book(&data, 10, MessageTrace::on_receive()).is_err());
    }

    #[test]
    fn handles_missing_timestamp() {
        let raw = r#"{"channel":"l2Book","data":{"coin":"HYPE","levels":[
            [{"px":"20.5","sz":"10","n":1}],[{"px":"20.6","sz":"10","n":1}]]}}"#;
        let HlMessage::L2Book { data } = serde_json::from_str(raw).unwrap() else {
            panic!()
        };
        let book = to_order_book(&data, 10, MessageTrace::on_receive()).unwrap();
        assert_eq!(book.trace.exchange_ts_ms, None);
        assert_eq!(book.trace.exchange_to_local_ms(), None);
    }

    #[test]
    fn classifies_control_messages() {
        assert!(matches!(
            serde_json::from_str::<HlMessage>(r#"{"channel":"pong"}"#).unwrap(),
            HlMessage::Pong
        ));
        assert!(matches!(
            serde_json::from_str::<HlMessage>(r#"{"channel":"subscriptionResponse","data":{}}"#)
                .unwrap(),
            HlMessage::SubscriptionResponse { .. }
        ));
        assert!(matches!(
            serde_json::from_str::<HlMessage>(r#"{"channel":"error","data":"bad"}"#).unwrap(),
            HlMessage::Error { .. }
        ));
        // 未知の channel でもパースは失敗しない
        assert!(matches!(
            serde_json::from_str::<HlMessage>(r#"{"channel":"trades","data":[]}"#).unwrap(),
            HlMessage::Other
        ));
    }

    #[test]
    fn subscribe_request_matches_api_format() {
        let json = serde_json::to_string(&SubscribeRequest::l2_book(Symbol::Hype)).unwrap();
        assert_eq!(
            json,
            r#"{"method":"subscribe","subscription":{"type":"l2Book","coin":"HYPE"}}"#
        );
    }
}
