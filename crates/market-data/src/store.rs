use core_types::{Dex, OrderBook, Symbol};
use dashmap::DashMap;
use rust_decimal::Decimal;

use crate::divergence::DivergenceSnapshot;

/// 銘柄ごとに全 DEX の最新板を保持する共有状態。
///
/// キー数は (DEX 数 × 銘柄数) で固定され、板は毎回上書きされるため、24 時間
/// 稼働させても容量が増え続けることはない。
#[derive(Debug, Default)]
pub struct BookStore {
    books: DashMap<(Dex, Symbol), OrderBook>,
}

impl BookStore {
    pub fn new() -> Self {
        BookStore {
            books: DashMap::new(),
        }
    }

    /// 最新板で上書きする。
    pub fn update(&self, book: OrderBook) {
        self.books.insert((book.dex, book.symbol), book);
    }

    pub fn get(&self, dex: Dex, symbol: Symbol) -> Option<OrderBook> {
        self.books.get(&(dex, symbol)).map(|b| b.clone())
    }

    pub fn len(&self) -> usize {
        self.books.len()
    }

    pub fn is_empty(&self) -> bool {
        self.books.is_empty()
    }

    /// 保持済みの板から 2 DEX 間の価格差を計算する。
    ///
    /// どちらかの板が未受信なら `None`。`trigger_dex` は「直前に更新された側」で、
    /// 内部処理レイテンシの計測対象になる。
    pub fn divergence(
        &self,
        symbol: Symbol,
        dex_a: Dex,
        dex_b: Dex,
        trigger_dex: Dex,
        vwap_notional: Option<Decimal>,
    ) -> Option<DivergenceSnapshot> {
        let book_a = self.books.get(&(dex_a, symbol))?;
        let book_b = self.books.get(&(dex_b, symbol))?;
        DivergenceSnapshot::compute(&book_a, &book_b, trigger_dex, vwap_notional)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{Level, MessageTrace, Price, Quantity};
    use rust_decimal_macros::dec;

    fn book(dex: Dex, symbol: Symbol, bid: Decimal, ask: Decimal) -> OrderBook {
        OrderBook::new(
            dex,
            symbol,
            vec![Level::new(Price(bid), Quantity(dec!(10)))],
            vec![Level::new(Price(ask), Quantity(dec!(10)))],
            MessageTrace::on_receive(),
        )
    }

    #[test]
    fn update_overwrites_same_key() {
        let store = BookStore::new();
        store.update(book(Dex::Hyperliquid, Symbol::Btc, dec!(100), dec!(101)));
        store.update(book(Dex::Hyperliquid, Symbol::Btc, dec!(200), dec!(201)));
        assert_eq!(store.len(), 1);
        assert_eq!(
            store.get(Dex::Hyperliquid, Symbol::Btc).unwrap().best_bid(),
            Some(Price(dec!(200)))
        );
    }

    #[test]
    fn divergence_requires_both_sides() {
        let store = BookStore::new();
        store.update(book(Dex::Hyperliquid, Symbol::Btc, dec!(100), dec!(101)));
        assert!(store
            .divergence(
                Symbol::Btc,
                Dex::Hyperliquid,
                Dex::EdgeX,
                Dex::Hyperliquid,
                None
            )
            .is_none());

        store.update(book(Dex::EdgeX, Symbol::Btc, dec!(110), dec!(111)));
        let s = store
            .divergence(
                Symbol::Btc,
                Dex::Hyperliquid,
                Dex::EdgeX,
                Dex::Hyperliquid,
                None,
            )
            .unwrap();
        assert!(s.raw_spread_bps < Decimal::ZERO, "B の方が高い");

        // 別銘柄は独立
        assert!(store
            .divergence(
                Symbol::Eth,
                Dex::Hyperliquid,
                Dex::EdgeX,
                Dex::Hyperliquid,
                None
            )
            .is_none());
    }
}
