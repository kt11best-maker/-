//! `subscribed` の全量スナップショット + `channel_data` の差分からローカルで
//! 板を再構築する。
//!
//! # size 0 は削除
//!
//! **この処理を誤ると板に古いレベルが永久に残留し、価格差計算が静かに壊れる。**
//! 発見が遅れる種類のバグなのでテストを必ず持つこと。
//!
//! # 順序検証
//!
//! `message_id` は接続ごとの論理オフセットで、1 ずつ増える。飛んだら欠損なので
//! 再購読して板を作り直す。ただし `message_id` は**接続内の全チャンネル共通**の
//! 連番なので、他のチャンネルを購読していると板だけを見れば飛んで見える。
//! 板チャンネルしか購読していない前提でのみ厳密検証する。

use std::collections::BTreeMap;

use core_types::{Dex, Level, MessageTrace, OrderBook, Price, Quantity, Symbol};
use rust_decimal::Decimal;

use crate::message::{OrderBookContents, PriceSize};

/// 差分適用の失敗。いずれも「再購読してスナップショットを取り直す」が対処。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyError {
    /// スナップショット未受信のまま差分が届いた。
    NotInitialized,
    /// `message_id` が飛んだ。
    MessageIdGap { expected: u64, got: u64 },
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::NotInitialized => write!(f, "スナップショット未受信のまま差分が届いた"),
            ApplyError::MessageIdGap { expected, got } => {
                write!(f, "message_id 欠損: got={got}, 期待値={expected}")
            }
        }
    }
}

impl std::error::Error for ApplyError {}

/// 銘柄 1 つ分のローカル板。
#[derive(Debug)]
pub struct DydxBookBuilder {
    symbol: Symbol,
    bids: BTreeMap<Decimal, Decimal>,
    asks: BTreeMap<Decimal, Decimal>,
    last_message_id: Option<u64>,
    initialized: bool,
    max_levels: usize,
}

impl DydxBookBuilder {
    pub fn new(symbol: Symbol, max_levels: usize) -> Self {
        DydxBookBuilder {
            symbol,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            last_message_id: None,
            initialized: false,
            max_levels: max_levels.max(1),
        }
    }

    pub fn symbol(&self) -> Symbol {
        self.symbol
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn last_message_id(&self) -> Option<u64> {
        self.last_message_id
    }

    pub fn reset(&mut self) {
        self.bids.clear();
        self.asks.clear();
        self.last_message_id = None;
        self.initialized = false;
    }

    /// 全量スナップショットで板を置き換える。
    ///
    /// **既存のローカル板を必ず捨ててから**構築し直す（差分の取りこぼしが
    /// 残らないようにするため）。
    pub fn apply_snapshot(&mut self, contents: &OrderBookContents, message_id: Option<u64>) {
        self.bids = to_side(&contents.bids);
        self.asks = to_side(&contents.asks);
        self.last_message_id = message_id;
        self.initialized = true;
        self.trim();
    }

    /// 差分を適用する。**size 0 は削除。**
    pub fn apply_update(
        &mut self,
        contents: &OrderBookContents,
        message_id: Option<u64>,
    ) -> Result<(), ApplyError> {
        if !self.initialized {
            return Err(ApplyError::NotInitialized);
        }
        // 両方揃っているときだけ検証する（message_id が無い形式なら検証を
        // 諦めて、定期的なスナップショットに委ねる）。
        if let (Some(last), Some(got)) = (self.last_message_id, message_id) {
            let expected = last + 1;
            if got != expected {
                return Err(ApplyError::MessageIdGap { expected, got });
            }
        }

        for level in &contents.bids {
            if let Some((price, size)) = level.parts() {
                upsert(&mut self.bids, price, size);
            }
        }
        for level in &contents.asks {
            if let Some((price, size)) = level.parts() {
                upsert(&mut self.asks, price, size);
            }
        }

        if message_id.is_some() {
            self.last_message_id = message_id;
        }
        self.trim();
        Ok(())
    }

    /// 現在の板を正規化済み [`OrderBook`] に変換する。
    ///
    /// dYdX の Indexer は取引所側タイムスタンプを板メッセージに含めないため、
    /// `exchange_ts_ms` は埋めない（`latency_ms` は空欄になる）。
    /// **鮮度は `staleness_delta_ms` で見ること。**
    pub fn to_order_book(&self, depth: usize, mut trace: MessageTrace) -> OrderBook {
        let bids: Vec<Level> = self
            .bids
            .iter()
            .rev()
            .take(depth)
            .map(|(p, q)| Level::new(Price(*p), Quantity(*q)))
            .collect();
        let asks: Vec<Level> = self
            .asks
            .iter()
            .take(depth)
            .map(|(p, q)| Level::new(Price(*p), Quantity(*q)))
            .collect();

        trace.mark_normalized();
        OrderBook::new(Dex::Dydx, self.symbol, bids, asks, trace)
    }

    fn trim(&mut self) {
        while self.bids.len() > self.max_levels {
            let Some(lowest) = self.bids.keys().next().copied() else {
                break;
            };
            self.bids.remove(&lowest);
        }
        while self.asks.len() > self.max_levels {
            let Some(highest) = self.asks.keys().next_back().copied() else {
                break;
            };
            self.asks.remove(&highest);
        }
    }
}

fn to_side(levels: &[PriceSize]) -> BTreeMap<Decimal, Decimal> {
    let mut side = BTreeMap::new();
    for level in levels {
        if let Some((price, size)) = level.parts() {
            if size > Decimal::ZERO {
                side.insert(price, size);
            }
        }
    }
    side
}

fn upsert(side: &mut BTreeMap<Decimal, Decimal>, price: Decimal, size: Decimal) {
    if size > Decimal::ZERO {
        side.insert(price, size);
    } else {
        // size 0 = 削除。ここを落とすと板が永久に残留する。
        side.remove(&price);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::DydxEnvelope;
    use rust_decimal_macros::dec;

    fn contents(json: &str) -> (OrderBookContents, Option<u64>) {
        let env: DydxEnvelope = serde_json::from_str(json).unwrap();
        (env.order_book().unwrap(), env.message_id())
    }

    const SNAPSHOT: &str = r#"{"type":"subscribed","channel":"v4_orderbook","id":"BTC-USD",
        "message_id":1,"contents":{
          "bids":[{"price":"36000.5","size":"1.5"},{"price":"36000.0","size":"3.0"}],
          "asks":[{"price":"36001.0","size":"2.0"},{"price":"36002.0","size":"1.0"}]}}"#;

    fn builder_with_snapshot() -> DydxBookBuilder {
        let mut b = DydxBookBuilder::new(Symbol::Btc, 100);
        let (c, id) = contents(SNAPSHOT);
        b.apply_snapshot(&c, id);
        b
    }

    fn apply(b: &mut DydxBookBuilder, json: &str) -> Result<(), ApplyError> {
        let (c, id) = contents(json);
        b.apply_update(&c, id)
    }

    fn book(b: &DydxBookBuilder) -> OrderBook {
        b.to_order_book(20, MessageTrace::on_receive())
    }

    #[test]
    fn snapshot_builds_sorted_book() {
        let b = builder_with_snapshot();
        let book = book(&b);
        assert_eq!(book.dex, Dex::Dydx);
        assert_eq!(book.symbol, Symbol::Btc);
        assert_eq!(book.best_bid(), Some(Price(dec!(36000.5))));
        assert_eq!(book.best_ask(), Some(Price(dec!(36001.0))));
        assert_eq!(b.last_message_id(), Some(1));
        book.validate().unwrap();
    }

    #[test]
    fn snapshot_resets_previous_state() {
        let mut b = builder_with_snapshot();
        // 別のスナップショットで完全に置き換わる（古いレベルは残らない）
        let (c, id) = contents(
            r#"{"type":"subscribed","channel":"v4_orderbook","id":"BTC-USD","message_id":9,
                "contents":{"bids":[["100.0","1.0"]],"asks":[["101.0","1.0"]]}}"#,
        );
        b.apply_snapshot(&c, id);

        let book = book(&b);
        assert_eq!(book.bids.len(), 1);
        assert_eq!(book.asks.len(), 1);
        assert_eq!(book.best_bid(), Some(Price(dec!(100.0))));
        assert_eq!(b.last_message_id(), Some(9));
    }

    #[test]
    fn zero_size_deletes_the_level() {
        // この挙動を誤ると板が永久に残留して価格差計算が壊れる
        let mut b = builder_with_snapshot();
        apply(
            &mut b,
            r#"{"type":"channel_data","channel":"v4_orderbook","id":"BTC-USD","message_id":2,
                "contents":{"bids":[["36000.0","0"]],"asks":[["36001.0","0"]]}}"#,
        )
        .unwrap();

        let book = book(&b);
        assert_eq!(book.bids.len(), 1, "36000.0 が消えている: {:?}", book.bids);
        assert!(book.bids.iter().all(|l| l.price != Price(dec!(36000.0))));
        assert_eq!(book.asks.len(), 1);
        assert_eq!(book.best_ask(), Some(Price(dec!(36002.0))));
    }

    #[test]
    fn update_inserts_and_replaces_levels() {
        let mut b = builder_with_snapshot();
        apply(
            &mut b,
            r#"{"type":"channel_data","channel":"v4_orderbook","id":"BTC-USD","message_id":2,
                "contents":{"bids":[["36000.75","0.5"]],"asks":[["36001.0","5"]]}}"#,
        )
        .unwrap();

        let book = book(&b);
        assert_eq!(book.best_bid(), Some(Price(dec!(36000.75))));
        assert_eq!(book.bids.len(), 3);
        // 同じ価格は上書き（加算ではない）
        assert_eq!(book.asks[0].quantity, Quantity(dec!(5)));
        assert_eq!(b.last_message_id(), Some(2));
    }

    #[test]
    fn deleting_unknown_level_is_harmless() {
        let mut b = builder_with_snapshot();
        apply(
            &mut b,
            r#"{"type":"channel_data","channel":"v4_orderbook","id":"BTC-USD","message_id":2,
                "contents":{"bids":[["1.0","0"]],"asks":[["999999.0","0"]]}}"#,
        )
        .unwrap();
        let book = book(&b);
        assert_eq!(book.bids.len(), 2);
        assert_eq!(book.asks.len(), 2);
    }

    #[test]
    fn detects_message_id_gap() {
        let mut b = builder_with_snapshot();
        assert_eq!(
            apply(
                &mut b,
                r#"{"type":"channel_data","channel":"v4_orderbook","id":"BTC-USD","message_id":5,
                    "contents":{"bids":[["36000.9","1"]],"asks":[]}}"#,
            ),
            Err(ApplyError::MessageIdGap {
                expected: 2,
                got: 5
            })
        );
        // 欠損した差分は板に反映されない
        assert_eq!(book(&b).best_bid(), Some(Price(dec!(36000.5))));
    }

    #[test]
    fn sequential_updates_keep_book_consistent() {
        let mut b = builder_with_snapshot();
        for (id, price, size) in [
            (2u64, "36000.6", "1"),
            (3, "36000.7", "2"),
            (4, "36000.6", "0"),
        ] {
            apply(
                &mut b,
                &format!(
                    r#"{{"type":"channel_data","channel":"v4_orderbook","id":"BTC-USD",
                        "message_id":{id},"contents":{{"bids":[["{price}","{size}"]],"asks":[]}}}}"#
                ),
            )
            .unwrap();
        }
        let book = book(&b);
        assert_eq!(book.best_bid(), Some(Price(dec!(36000.7))));
        assert!(book.bids.iter().all(|l| l.price != Price(dec!(36000.6))));
        assert_eq!(b.last_message_id(), Some(4));
    }

    #[test]
    fn rejects_update_before_snapshot() {
        let mut b = DydxBookBuilder::new(Symbol::Eth, 100);
        assert_eq!(
            apply(
                &mut b,
                r#"{"type":"channel_data","channel":"v4_orderbook","id":"ETH-USD","message_id":1,
                    "contents":{"bids":[["2000","1"]],"asks":[]}}"#,
            ),
            Err(ApplyError::NotInitialized)
        );
    }

    #[test]
    fn crossed_book_is_kept_not_discarded() {
        // dYdX ではクロスが構造上正常に起こる。捨てずにフラグで扱う。
        let mut b = builder_with_snapshot();
        apply(
            &mut b,
            r#"{"type":"channel_data","channel":"v4_orderbook","id":"BTC-USD","message_id":2,
                "contents":{"bids":[["36001.5","1.0"]],"asks":[]}}"#,
        )
        .unwrap();

        let book = book(&b);
        assert!(book.is_crossed(), "bid 36001.5 > ask 36001.0");
        // ソート順・数量は正しいので、板としては壊れていない
        book.validate_allowing_crossed().unwrap();
        assert!(book.validate().is_err());
    }

    #[test]
    fn zero_size_levels_in_snapshot_are_ignored() {
        let mut b = DydxBookBuilder::new(Symbol::Sol, 100);
        let (c, id) = contents(
            r#"{"type":"subscribed","channel":"v4_orderbook","id":"SOL-USD","message_id":1,
                "contents":{"bids":[["100","1"],["99","0"]],"asks":[["101","1"]]}}"#,
        );
        b.apply_snapshot(&c, id);
        assert_eq!(book(&b).bids.len(), 1);
    }

    #[test]
    fn trims_to_max_levels() {
        let mut b = DydxBookBuilder::new(Symbol::Btc, 1);
        let (c, id) = contents(SNAPSHOT);
        b.apply_snapshot(&c, id);
        let book = book(&b);
        assert_eq!(book.bids.len(), 1);
        assert_eq!(book.asks.len(), 1);
        assert_eq!(book.best_bid(), Some(Price(dec!(36000.5))));
    }

    #[test]
    fn reset_clears_state() {
        let mut b = builder_with_snapshot();
        b.reset();
        assert!(!b.is_initialized());
        assert_eq!(b.last_message_id(), None);
        assert!(book(&b).best_bid().is_none());
    }
}
