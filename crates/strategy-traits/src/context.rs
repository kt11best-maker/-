use std::time::Instant;

use core_types::{Dex, FeeSchedule, Symbol, BPS_DENOMINATOR};
use market_data::{BookStore, FundingStore};
use rust_decimal::Decimal;

/// 戦略に渡す市場状態のスナップショット。
///
/// 板・ファンディング・手数料表への参照だけを持ち、自身は状態を持たない。
/// 戦略の判定を純粋関数に保つための入れ物。
pub struct MarketContext<'a> {
    pub books: &'a BookStore,
    pub funding: &'a FundingStore,
    pub fees: &'a FeeSchedule,
    /// 評価対象の銘柄。
    pub symbols: &'a [Symbol],
    /// 評価対象の DEX ペア（`Dex` の宣言順に正規化済み）。
    pub pairs: &'a [(Dex, Dex)],
    pub now_wall_ms: u64,
    pub now: Instant,
}

impl<'a> MarketContext<'a> {
    /// 「`long_dex` で買い、`short_dex` で売る」方向から見た価格差（bps, 符号付き）。
    ///
    /// - **正** = ショート側の方が高い = 建てる方向として**有利**
    /// - **負** = ロング側の方が高い = 建てる方向として**不利**
    ///
    /// 両建てなので価格の絶対値は損益に無関係だが、**2 DEX 間の価格差が建てた時と
    /// 決済した時で変わると、その差分がそのまま損益になる**。エントリー判定では
    /// このフィルタを必ず通すこと。
    ///
    /// mid 価格ベース。どちらかの板が未受信なら `None`。
    pub fn signed_basis_bps(
        &self,
        symbol: Symbol,
        long_dex: Dex,
        short_dex: Dex,
    ) -> Option<Decimal> {
        let long_book = self.books.get(long_dex, symbol)?;
        let short_book = self.books.get(short_dex, symbol)?;
        let long_mid = long_book.mid()?;
        let short_mid = short_book.mid()?;

        let reference = (long_mid.0 + short_mid.0) / Decimal::TWO;
        if reference <= Decimal::ZERO {
            return None;
        }
        // ショート側が高いほど有利（高く売って安く買える）
        Some((short_mid.0 - long_mid.0) / reference * BPS_DENOMINATOR)
    }

    /// 2 DEX の板の鮮度差（ms, 符号付き）。
    ///
    /// 片方だけ古いと見かけ上の乖離が出るため、判定前にこれで足切りする。
    pub fn staleness_delta_ms(&self, symbol: Symbol, dex_a: Dex, dex_b: Dex) -> Option<i64> {
        let a = self.books.get(dex_a, symbol)?;
        let b = self.books.get(dex_b, symbol)?;
        Some(a.trace.received_wall_ms as i64 - b.trace.received_wall_ms as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{Level, MessageTrace, OrderBook, Price, Quantity};
    use rust_decimal_macros::dec;

    fn book(dex: Dex, bid: Decimal, ask: Decimal, wall_ms: u64) -> OrderBook {
        let mut trace = MessageTrace::on_receive();
        trace.received_wall_ms = wall_ms;
        OrderBook::new(
            dex,
            Symbol::Btc,
            vec![Level::new(Price(bid), Quantity(dec!(10)))],
            vec![Level::new(Price(ask), Quantity(dec!(10)))],
            trace,
        )
    }

    struct Fixture {
        books: BookStore,
        funding: FundingStore,
        fees: FeeSchedule,
        symbols: Vec<Symbol>,
        pairs: Vec<(Dex, Dex)>,
    }

    impl Fixture {
        fn ctx(&self) -> MarketContext<'_> {
            MarketContext {
                books: &self.books,
                funding: &self.funding,
                fees: &self.fees,
                symbols: &self.symbols,
                pairs: &self.pairs,
                now_wall_ms: 1_700_000_000_000,
                now: Instant::now(),
            }
        }
    }

    fn fixture(hl: (Decimal, Decimal, u64), lighter: (Decimal, Decimal, u64)) -> Fixture {
        let books = BookStore::new();
        books.update(book(Dex::Hyperliquid, hl.0, hl.1, hl.2));
        books.update(book(Dex::Lighter, lighter.0, lighter.1, lighter.2));
        Fixture {
            books,
            funding: FundingStore::new(),
            fees: FeeSchedule::new(),
            symbols: vec![Symbol::Btc],
            pairs: vec![(Dex::Hyperliquid, Dex::Lighter)],
        }
    }

    #[test]
    fn basis_is_positive_when_short_side_is_richer() {
        // Lighter が高い → Lighter でショート・Hyperliquid でロングが有利
        let f = fixture(
            (dec!(60000), dec!(60001), 1_000),
            (dec!(60030), dec!(60031), 1_000),
        );
        let ctx = f.ctx();

        let basis = ctx
            .signed_basis_bps(Symbol::Btc, Dex::Hyperliquid, Dex::Lighter)
            .unwrap();
        assert!(basis > Decimal::ZERO, "{basis}");
        // (60030.5 - 60000.5) / 60015.5 * 10000 ≒ 5.0 bps
        assert!((basis - dec!(4.9987)).abs() < dec!(0.01), "{basis}");

        // 逆向きに建てると符号が反転する（不利）
        let reversed = ctx
            .signed_basis_bps(Symbol::Btc, Dex::Lighter, Dex::Hyperliquid)
            .unwrap();
        assert_eq!(reversed, -basis);
    }

    #[test]
    fn basis_is_none_without_both_books() {
        let books = BookStore::new();
        books.update(book(Dex::Hyperliquid, dec!(60000), dec!(60001), 1_000));
        let f = Fixture {
            books,
            funding: FundingStore::new(),
            fees: FeeSchedule::new(),
            symbols: vec![Symbol::Btc],
            pairs: vec![(Dex::Hyperliquid, Dex::Lighter)],
        };
        assert!(f
            .ctx()
            .signed_basis_bps(Symbol::Btc, Dex::Hyperliquid, Dex::Lighter)
            .is_none());
    }

    #[test]
    fn staleness_delta_is_signed() {
        let f = fixture(
            (dec!(60000), dec!(60001), 5_000),
            (dec!(60000), dec!(60001), 3_800),
        );
        let ctx = f.ctx();
        assert_eq!(
            ctx.staleness_delta_ms(Symbol::Btc, Dex::Hyperliquid, Dex::Lighter),
            Some(1_200)
        );
        assert_eq!(
            ctx.staleness_delta_ms(Symbol::Btc, Dex::Lighter, Dex::Hyperliquid),
            Some(-1_200)
        );
    }
}
