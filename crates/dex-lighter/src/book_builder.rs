//! 購読時スナップショット + 差分からローカルで板を再構築する。
//!
//! 順序検証は **`begin_nonce` / `nonce`** で行う。現在の更新の `begin_nonce` が
//! 直前の更新の `nonce` と一致しなければ欠損なので、再購読して板を作り直す。
//!
//! `offset` も各更新で増加するが、API サーバーに紐づく値であり連続性が保証
//! されない（再接続で別サーバーにルーティングされると大きく変動する）。
//! **順序検証に `offset` を使ってはいけない。**

use std::collections::BTreeMap;

use core_types::{Dex, Level, MessageTrace, OrderBook, Price, Quantity, Symbol};
use rust_decimal::Decimal;

use crate::message::OrderBookPayload;

/// 差分適用の失敗。いずれも「再購読してスナップショットを取り直す」が対処。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyError {
    /// スナップショット未受信のまま差分が届いた。
    NotInitialized,
    /// `begin_nonce` が直前の `nonce` と一致しない。
    NonceGap { expected: u64, got: u64 },
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::NotInitialized => write!(f, "スナップショット未受信のまま差分が届いた"),
            ApplyError::NonceGap { expected, got } => {
                write!(f, "nonce 欠損: begin_nonce={got}, 期待値={expected}")
            }
        }
    }
}

impl std::error::Error for ApplyError {}

/// 銘柄 1 つ分のローカル板。
#[derive(Debug)]
pub struct LighterBookBuilder {
    symbol: Symbol,
    market_index: u32,
    bids: BTreeMap<Decimal, Decimal>,
    asks: BTreeMap<Decimal, Decimal>,
    last_nonce: Option<u64>,
    initialized: bool,
    max_levels: usize,
    exchange_ts_ms: Option<u64>,
}

impl LighterBookBuilder {
    pub fn new(symbol: Symbol, market_index: u32, max_levels: usize) -> Self {
        LighterBookBuilder {
            symbol,
            market_index,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            last_nonce: None,
            initialized: false,
            max_levels: max_levels.max(1),
            exchange_ts_ms: None,
        }
    }

    pub fn symbol(&self) -> Symbol {
        self.symbol
    }

    pub fn market_index(&self) -> u32 {
        self.market_index
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn last_nonce(&self) -> Option<u64> {
        self.last_nonce
    }

    pub fn reset(&mut self) {
        self.bids.clear();
        self.asks.clear();
        self.last_nonce = None;
        self.initialized = false;
        self.exchange_ts_ms = None;
    }

    /// 全量スナップショットで板を置き換える。
    pub fn apply_snapshot(
        &mut self,
        payload: &OrderBookPayload,
        nonce: Option<u64>,
        exchange_ts_ms: Option<u64>,
    ) {
        self.bids = to_side(&payload.bids);
        self.asks = to_side(&payload.asks);
        self.last_nonce = nonce;
        self.initialized = true;
        self.exchange_ts_ms = exchange_ts_ms;
        self.trim();
    }

    /// 差分を適用する。数量 0 は削除。
    pub fn apply_update(
        &mut self,
        payload: &OrderBookPayload,
        begin_nonce: Option<u64>,
        nonce: Option<u64>,
        exchange_ts_ms: Option<u64>,
    ) -> Result<(), ApplyError> {
        if !self.initialized {
            return Err(ApplyError::NotInitialized);
        }
        // 両方揃っているときだけ検証する（片方でも欠ける形式なら、検証自体を
        // 諦めて再購読時のスナップショットに委ねる）。
        if let (Some(expected), Some(got)) = (self.last_nonce, begin_nonce) {
            if expected != got {
                return Err(ApplyError::NonceGap { expected, got });
            }
        }

        for level in &payload.bids {
            if let Some((price, size)) = level.parts() {
                upsert(&mut self.bids, price, size);
            }
        }
        for level in &payload.asks {
            if let Some((price, size)) = level.parts() {
                upsert(&mut self.asks, price, size);
            }
        }

        if let Some(n) = nonce {
            self.last_nonce = Some(n);
        }
        if exchange_ts_ms.is_some() {
            self.exchange_ts_ms = exchange_ts_ms;
        }
        self.trim();
        Ok(())
    }

    /// 現在の板を正規化済み [`OrderBook`] に変換する。
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

        trace.set_exchange_ts_ms(self.exchange_ts_ms);
        trace.mark_normalized();
        OrderBook::new(Dex::Lighter, self.symbol, bids, asks, trace)
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

fn to_side(levels: &[crate::message::PriceSize]) -> BTreeMap<Decimal, Decimal> {
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
        side.remove(&price);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::LighterEnvelope;
    use rust_decimal_macros::dec;

    fn envelope(json: &str) -> LighterEnvelope {
        serde_json::from_str(json).unwrap()
    }

    fn snapshot_env() -> LighterEnvelope {
        envelope(
            r#"{"type":"subscribed/order_book","channel":"order_book:1",
                "last_updated_at":1700000000000,"offset":42,
                "order_book":{"code":0,"nonce":100,
                  "bids":[{"price":"36000.5","size":"1.5"},{"price":"36000.0","size":"3.0"}],
                  "asks":[{"price":"36001.0","size":"2.0"},{"price":"36002.0","size":"1.0"}]}}"#,
        )
    }

    fn builder_with_snapshot() -> LighterBookBuilder {
        let mut b = LighterBookBuilder::new(Symbol::Btc, 1, 100);
        let env = snapshot_env();
        b.apply_snapshot(
            env.order_book.as_ref().unwrap(),
            env.nonce(),
            env.exchange_ts_ms(),
        );
        b
    }

    fn apply(b: &mut LighterBookBuilder, env: &LighterEnvelope) -> Result<(), ApplyError> {
        b.apply_update(
            env.order_book.as_ref().unwrap(),
            env.begin_nonce(),
            env.nonce(),
            env.exchange_ts_ms(),
        )
    }

    fn book(b: &LighterBookBuilder) -> OrderBook {
        b.to_order_book(20, MessageTrace::on_receive())
    }

    #[test]
    fn snapshot_builds_sorted_book() {
        let b = builder_with_snapshot();
        let book = book(&b);
        assert_eq!(book.dex, Dex::Lighter);
        assert_eq!(book.symbol, Symbol::Btc);
        assert_eq!(book.best_bid(), Some(Price(dec!(36000.5))));
        assert_eq!(book.best_ask(), Some(Price(dec!(36001.0))));
        assert_eq!(book.trace.exchange_ts_ms, Some(1700000000000));
        assert_eq!(b.last_nonce(), Some(100));
        assert_eq!(b.market_index(), 1);
        book.validate().unwrap();
    }

    #[test]
    fn update_applies_inserts_and_deletes() {
        let mut b = builder_with_snapshot();
        let env = envelope(
            r#"{"type":"update/order_book","channel":"order_book:1",
                "last_updated_at":1700000000050,
                "order_book":{"begin_nonce":100,"nonce":101,
                  "bids":[{"price":"36000.75","size":"0.5"},{"price":"36000.0","size":"0"}],
                  "asks":[{"price":"36001.0","size":"5"}]}}"#,
        );
        apply(&mut b, &env).unwrap();

        let book = book(&b);
        assert_eq!(book.best_bid(), Some(Price(dec!(36000.75))));
        // size 0 は削除
        assert!(book.bids.iter().all(|l| l.price != Price(dec!(36000.0))));
        assert_eq!(book.asks[0].quantity, Quantity(dec!(5)));
        assert_eq!(b.last_nonce(), Some(101));
        assert_eq!(book.trace.exchange_ts_ms, Some(1700000000050));
        book.validate().unwrap();
    }

    #[test]
    fn detects_nonce_gap() {
        let mut b = builder_with_snapshot();
        let env = envelope(
            r#"{"type":"update/order_book","channel":"order_book:1",
                "order_book":{"begin_nonce":105,"nonce":106,
                  "bids":[{"price":"36000.9","size":"1"}],"asks":[]}}"#,
        );
        assert_eq!(
            apply(&mut b, &env),
            Err(ApplyError::NonceGap {
                expected: 100,
                got: 105
            })
        );
        // 欠損した差分は板に反映されない
        assert_eq!(book(&b).best_bid(), Some(Price(dec!(36000.5))));
    }

    #[test]
    fn offset_jumps_do_not_break_ordering() {
        // 再接続で別サーバーに繋がると offset は大きく飛ぶ。nonce が繋がって
        // いる限り、これは正常として扱わなければならない。
        let mut b = builder_with_snapshot();
        let env = envelope(
            r#"{"type":"update/order_book","channel":"order_book:1","offset":999999999,
                "order_book":{"begin_nonce":100,"nonce":101,"offset":1,
                  "bids":[{"price":"36000.75","size":"0.5"}],"asks":[]}}"#,
        );
        apply(&mut b, &env).unwrap();
        assert_eq!(book(&b).best_bid(), Some(Price(dec!(36000.75))));
    }

    #[test]
    fn rejects_update_before_snapshot() {
        let mut b = LighterBookBuilder::new(Symbol::Eth, 0, 100);
        let env = envelope(
            r#"{"type":"update/order_book","order_book":{"begin_nonce":1,"nonce":2,
                "bids":[{"price":"2000","size":"1"}],"asks":[]}}"#,
        );
        assert_eq!(apply(&mut b, &env), Err(ApplyError::NotInitialized));
    }

    #[test]
    fn sequential_updates_keep_book_consistent() {
        let mut b = builder_with_snapshot();
        for (begin, nonce, price, size) in [
            (100u64, 101u64, "36000.6", "1"),
            (101, 102, "36000.7", "2"),
            (102, 103, "36000.6", "0"),
        ] {
            let env = envelope(&format!(
                r#"{{"type":"update/order_book","order_book":{{"begin_nonce":{begin},"nonce":{nonce},
                    "bids":[{{"price":"{price}","size":"{size}"}}],"asks":[]}}}}"#
            ));
            apply(&mut b, &env).unwrap();
        }
        let book = book(&b);
        assert_eq!(book.best_bid(), Some(Price(dec!(36000.7))));
        assert!(book.bids.iter().all(|l| l.price != Price(dec!(36000.6))));
        assert_eq!(b.last_nonce(), Some(103));
    }

    #[test]
    fn deleting_unknown_level_is_harmless() {
        let mut b = builder_with_snapshot();
        let env = envelope(
            r#"{"type":"update/order_book","order_book":{"begin_nonce":100,"nonce":101,
                "bids":[{"price":"1.0","size":"0"}],"asks":[{"price":"999999.0","size":"0"}]}}"#,
        );
        apply(&mut b, &env).unwrap();
        let book = book(&b);
        assert_eq!(book.bids.len(), 2);
        assert_eq!(book.asks.len(), 2);
    }

    #[test]
    fn reset_clears_state() {
        let mut b = builder_with_snapshot();
        b.reset();
        assert!(!b.is_initialized());
        assert_eq!(b.last_nonce(), None);
        assert!(book(&b).best_bid().is_none());
    }

    #[test]
    fn trims_to_max_levels() {
        let mut b = LighterBookBuilder::new(Symbol::Btc, 1, 1);
        let env = snapshot_env();
        b.apply_snapshot(
            env.order_book.as_ref().unwrap(),
            env.nonce(),
            env.exchange_ts_ms(),
        );
        let book = book(&b);
        assert_eq!(book.bids.len(), 1);
        assert_eq!(book.asks.len(), 1);
        assert_eq!(book.best_bid(), Some(Price(dec!(36000.5))));
    }

    #[test]
    fn zero_size_levels_in_snapshot_are_ignored() {
        let mut b = LighterBookBuilder::new(Symbol::Sol, 3, 100);
        let env = envelope(
            r#"{"order_book":{"nonce":1,
                "bids":[{"price":"100","size":"1"},{"price":"99","size":"0"}],
                "asks":[{"price":"101","size":"1"}]}}"#,
        );
        b.apply_snapshot(
            env.order_book.as_ref().unwrap(),
            env.nonce(),
            env.exchange_ts_ms(),
        );
        assert_eq!(book(&b).bids.len(), 1);
    }
}
