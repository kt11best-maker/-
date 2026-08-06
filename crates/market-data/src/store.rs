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

    /// 複数ペアの価格差をまとめて計算する。
    ///
    /// 板が揃っていないペア（= その銘柄がその DEX に存在しない、まだ未受信）は
    /// **正常系として黙ってスキップ**する。「全銘柄が全 DEX に存在する」前提を
    /// 置かないための入口がここ。
    pub fn divergences(
        &self,
        symbol: Symbol,
        pairs: &[(Dex, Dex)],
        trigger_dex: Dex,
        vwap_notional: Option<Decimal>,
    ) -> Vec<DivergenceSnapshot> {
        pairs
            .iter()
            .filter_map(|(a, b)| self.divergence(symbol, *a, *b, trigger_dex, vwap_notional))
            .collect()
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

    #[test]
    fn divergences_skip_pairs_whose_book_is_missing() {
        let store = BookStore::new();
        // HYPE は Hyperliquid と Aster にだけ存在する状況を作る
        store.update(book(Dex::Hyperliquid, Symbol::Hype, dec!(20), dec!(20.1)));
        store.update(book(Dex::Aster, Symbol::Hype, dec!(21), dec!(21.1)));

        let pairs = crate::pairs::pairs_involving(Dex::Aster, &Dex::ALL);
        assert_eq!(pairs.len(), 3, "Aster を含むペアは 3 通り");

        let snapshots = store.divergences(Symbol::Hype, &pairs, Dex::Aster, None);
        // edgeX / Lighter の板が無いペアはスキップされる
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].dex_a, Dex::Hyperliquid);
        assert_eq!(snapshots[0].dex_b, Dex::Aster);
        assert_eq!(snapshots[0].trigger_dex, Dex::Aster);
    }

    #[test]
    fn divergences_cover_all_pairs_when_every_dex_has_the_book() {
        let store = BookStore::new();
        for (i, dex) in Dex::ALL.iter().enumerate() {
            let base = dec!(100) + Decimal::from(i);
            store.update(book(*dex, Symbol::Btc, base, base + dec!(1)));
        }
        let all = store.divergences(
            Symbol::Btc,
            &crate::pairs::dex_pairs(&Dex::ALL),
            Dex::Hyperliquid,
            None,
        );
        assert_eq!(all.len(), 6, "4 DEX なら 6 ペア");

        // トリガー側を含むペアだけに絞ると 3 件
        let involving = store.divergences(
            Symbol::Btc,
            &crate::pairs::pairs_involving(Dex::Hyperliquid, &Dex::ALL),
            Dex::Hyperliquid,
            None,
        );
        assert_eq!(involving.len(), 3);
    }
}
