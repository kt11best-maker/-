use rust_decimal::Decimal;

use crate::num::{Price, Quantity, BPS_DENOMINATOR};
use crate::symbol::{Dex, Symbol};
use crate::trace::MessageTrace;

/// 板のサイド。
///
/// [`OrderBook::vwap_for_size`] や [`OrderBook::max_size_within_slippage`] では
/// 「**食い込む側の板**」を指す:
/// - `Side::Ask` … 買う場合（ask を食う）
/// - `Side::Bid` … 売る場合（bid を食う）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Bid,
    Ask,
}

impl Side {
    pub fn opposite(&self) -> Side {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Side::Bid => "bid",
            Side::Ask => "ask",
        }
    }
}

/// 板の 1 レベル。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Level {
    pub price: Price,
    pub quantity: Quantity,
}

impl Level {
    pub fn new(price: Price, quantity: Quantity) -> Self {
        Level { price, quantity }
    }
}

/// 正規化された板スナップショット。
#[derive(Debug, Clone)]
pub struct OrderBook {
    pub dex: Dex,
    pub symbol: Symbol,
    /// 高い順にソート済み。
    pub bids: Vec<Level>,
    /// 安い順にソート済み。
    pub asks: Vec<Level>,
    pub trace: MessageTrace,
}

/// 板の整合性エラー（差分再構築のバグ検知用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookIntegrityError {
    /// bid が降順になっていない。
    BidsNotDescending,
    /// ask が昇順になっていない。
    AsksNotAscending,
    /// best_bid >= best_ask（板がクロスしている）。
    Crossed { best_bid: Price, best_ask: Price },
    /// 数量が 0 以下のレベルが残っている。
    NonPositiveQuantity,
}

impl std::fmt::Display for BookIntegrityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BookIntegrityError::BidsNotDescending => write!(f, "bids が降順でない"),
            BookIntegrityError::AsksNotAscending => write!(f, "asks が昇順でない"),
            BookIntegrityError::Crossed { best_bid, best_ask } => {
                write!(f, "板がクロス: best_bid={best_bid} >= best_ask={best_ask}")
            }
            BookIntegrityError::NonPositiveQuantity => write!(f, "数量が 0 以下のレベルが存在"),
        }
    }
}

impl std::error::Error for BookIntegrityError {}

impl OrderBook {
    pub fn new(
        dex: Dex,
        symbol: Symbol,
        bids: Vec<Level>,
        asks: Vec<Level>,
        trace: MessageTrace,
    ) -> Self {
        OrderBook {
            dex,
            symbol,
            bids,
            asks,
            trace,
        }
    }

    pub fn best_bid(&self) -> Option<Price> {
        self.bids.first().map(|l| l.price)
    }

    pub fn best_ask(&self) -> Option<Price> {
        self.asks.first().map(|l| l.price)
    }

    /// (best_bid + best_ask) / 2。片側でも欠けていれば `None`。
    pub fn mid(&self) -> Option<Price> {
        match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) => Some(Price((b.0 + a.0) / Decimal::TWO)),
            _ => None,
        }
    }

    /// best_ask - best_bid（bps）。
    pub fn spread_bps(&self) -> Option<Decimal> {
        let (bid, ask, mid) = (self.best_bid()?, self.best_ask()?, self.mid()?);
        if mid.0.is_zero() {
            return None;
        }
        Some((ask.0 - bid.0) / mid.0 * BPS_DENOMINATOR)
    }

    fn levels(&self, side: Side) -> &[Level] {
        match side {
            Side::Bid => &self.bids,
            Side::Ask => &self.asks,
        }
    }

    /// `size` を `side` の板に食い込ませて約定させた場合の平均約定価格。
    ///
    /// 板の深さが足りない場合は `None`（= そのサイズは執行できない）。
    /// `size <= 0` も `None`。
    pub fn vwap_for_size(&self, side: Side, size: Quantity) -> Option<Price> {
        if !size.is_positive() {
            return None;
        }
        let mut remaining = size.0;
        let mut notional = Decimal::ZERO;
        for level in self.levels(side) {
            if !level.quantity.is_positive() {
                continue;
            }
            let take = remaining.min(level.quantity.0);
            notional += take * level.price.0;
            remaining -= take;
            if remaining <= Decimal::ZERO {
                break;
            }
        }
        if remaining > Decimal::ZERO {
            // 板の深さ不足。ここで Some を返すと執行可能量を過大評価するため必ず None。
            return None;
        }
        Some(Price(notional / size.0))
    }

    /// best 価格から `max_slippage_bps` 以内に収まる最大数量。
    ///
    /// 「その価格帯までに並んでいる数量の合計」であり、板の厚さ指標として使う。
    /// 板が空の場合は 0。
    pub fn max_size_within_slippage(&self, side: Side, max_slippage_bps: u32) -> Quantity {
        let levels = self.levels(side);
        let Some(best) = levels.first().map(|l| l.price.0) else {
            return Quantity::ZERO;
        };
        let ratio = Decimal::from(max_slippage_bps) / BPS_DENOMINATOR;
        // 買い（ask を食う）は価格が上がる方向、売り（bid を食う）は下がる方向が許容範囲。
        let bound = match side {
            Side::Ask => best * (Decimal::ONE + ratio),
            Side::Bid => best * (Decimal::ONE - ratio),
        };

        let mut total = Decimal::ZERO;
        for level in levels {
            let within = match side {
                Side::Ask => level.price.0 <= bound,
                Side::Bid => level.price.0 >= bound,
            };
            if !within {
                break;
            }
            if level.quantity.is_positive() {
                total += level.quantity.0;
            }
        }
        Quantity(total)
    }

    /// `size` を執行したときの best 価格からのスリッページ（bps, 常に非負）。
    pub fn slippage_bps_for_size(&self, side: Side, size: Quantity) -> Option<Decimal> {
        let best = match side {
            Side::Bid => self.best_bid()?,
            Side::Ask => self.best_ask()?,
        };
        if best.0.is_zero() {
            return None;
        }
        let vwap = self.vwap_for_size(side, size)?;
        let diff = match side {
            // 売り: best より安く約定するほど不利
            Side::Bid => best.0 - vwap.0,
            // 買い: best より高く約定するほど不利
            Side::Ask => vwap.0 - best.0,
        };
        Some(diff / best.0 * BPS_DENOMINATOR)
    }

    /// best_bid >= best_ask か。
    ///
    /// 多くの DEX ではデータ破損のサインだが、**dYdX v4 では構造上正常に
    /// 起こる**（中央集権的なオーダーブックを持たないため）。
    /// そのため「エラー」ではなく「状態」として取り出せるようにしてある。
    pub fn is_crossed(&self) -> bool {
        match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) => b >= a,
            _ => false,
        }
    }

    /// 板の整合性チェック。差分更新の再構築ロジックのバグを早期に検知する。
    ///
    /// クロスもエラーとして扱う。クロスが正常に起こる DEX では
    /// [`OrderBook::validate_allowing_crossed`] を使うこと。
    pub fn validate(&self) -> Result<(), BookIntegrityError> {
        self.validate_allowing_crossed()?;
        if let (Some(b), Some(a)) = (self.best_bid(), self.best_ask()) {
            if b >= a {
                return Err(BookIntegrityError::Crossed {
                    best_bid: b,
                    best_ask: a,
                });
            }
        }
        Ok(())
    }

    /// クロス以外の整合性チェック。
    ///
    /// dYdX のようにクロスが正常に起こる DEX 向け。**クロスした板を捨てては
    /// いけない**（クロスの発生頻度自体がフェーズ1 の計測対象）。ソート順・
    /// 数量の異常は依然としてバグのサインなので検査する。
    pub fn validate_allowing_crossed(&self) -> Result<(), BookIntegrityError> {
        if self.bids.windows(2).any(|w| w[0].price < w[1].price) {
            return Err(BookIntegrityError::BidsNotDescending);
        }
        if self.asks.windows(2).any(|w| w[0].price > w[1].price) {
            return Err(BookIntegrityError::AsksNotAscending);
        }
        if self
            .bids
            .iter()
            .chain(self.asks.iter())
            .any(|l| !l.quantity.is_positive())
        {
            return Err(BookIntegrityError::NonPositiveQuantity);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn lvl(p: Decimal, q: Decimal) -> Level {
        Level::new(Price(p), Quantity(q))
    }

    /// bids: 100 x1, 99 x2, 98 x3 / asks: 101 x1, 102 x2, 103 x3
    fn book() -> OrderBook {
        OrderBook::new(
            Dex::Hyperliquid,
            Symbol::Btc,
            vec![
                lvl(dec!(100), dec!(1)),
                lvl(dec!(99), dec!(2)),
                lvl(dec!(98), dec!(3)),
            ],
            vec![
                lvl(dec!(101), dec!(1)),
                lvl(dec!(102), dec!(2)),
                lvl(dec!(103), dec!(3)),
            ],
            MessageTrace::on_receive(),
        )
    }

    fn empty_book() -> OrderBook {
        OrderBook::new(
            Dex::EdgeX,
            Symbol::Eth,
            vec![],
            vec![],
            MessageTrace::on_receive(),
        )
    }

    #[test]
    fn best_and_mid() {
        let b = book();
        assert_eq!(b.best_bid(), Some(Price(dec!(100))));
        assert_eq!(b.best_ask(), Some(Price(dec!(101))));
        assert_eq!(b.mid(), Some(Price(dec!(100.5))));
        assert!(empty_book().mid().is_none());
    }

    #[test]
    fn vwap_within_first_level() {
        let b = book();
        // 最良気配の中だけで埋まる場合は best 価格そのもの
        assert_eq!(
            b.vwap_for_size(Side::Ask, Quantity(dec!(0.5))),
            Some(Price(dec!(101)))
        );
        assert_eq!(
            b.vwap_for_size(Side::Bid, Quantity(dec!(1))),
            Some(Price(dec!(100)))
        );
    }

    #[test]
    fn vwap_across_levels() {
        let b = book();
        // 買い 2.0: 101x1 + 102x1 = 203 / 2 = 101.5
        assert_eq!(
            b.vwap_for_size(Side::Ask, Quantity(dec!(2))),
            Some(Price(dec!(101.5)))
        );
        // 買い 3.0: 101x1 + 102x2 = 305 / 3
        let vwap = b.vwap_for_size(Side::Ask, Quantity(dec!(3))).unwrap();
        assert!((vwap.0 - dec!(101.666666)).abs() < dec!(0.000001), "{vwap}");
        // 売り 2.0: 100x1 + 99x1 = 199 / 2 = 99.5
        assert_eq!(
            b.vwap_for_size(Side::Bid, Quantity(dec!(2))),
            Some(Price(dec!(99.5)))
        );
    }

    #[test]
    fn vwap_consumes_entire_book_exactly() {
        let b = book();
        // ask 合計 6.0 をちょうど食い切る: (101 + 204 + 309) / 6 = 102.333...
        let vwap = b.vwap_for_size(Side::Ask, Quantity(dec!(6))).unwrap();
        assert!((vwap.0 - dec!(102.333333)).abs() < dec!(0.000001), "{vwap}");
    }

    #[test]
    fn vwap_returns_none_when_book_too_thin() {
        let b = book();
        // ask 合計は 6.0 しかない
        assert_eq!(b.vwap_for_size(Side::Ask, Quantity(dec!(6.0001))), None);
        assert_eq!(b.vwap_for_size(Side::Bid, Quantity(dec!(100))), None);
        assert_eq!(
            empty_book().vwap_for_size(Side::Ask, Quantity(dec!(1))),
            None
        );
    }

    #[test]
    fn vwap_rejects_non_positive_size() {
        let b = book();
        assert_eq!(b.vwap_for_size(Side::Ask, Quantity(dec!(0))), None);
        assert_eq!(b.vwap_for_size(Side::Ask, Quantity(dec!(-1))), None);
    }

    #[test]
    fn max_size_within_slippage_ask_side() {
        let b = book();
        // best_ask=101。0bps → 101 のみ = 1.0
        assert_eq!(b.max_size_within_slippage(Side::Ask, 0), Quantity(dec!(1)));
        // 100bps(1%) → 101 * 1.01 = 102.01 まで → 101 と 102 = 3.0
        assert_eq!(
            b.max_size_within_slippage(Side::Ask, 100),
            Quantity(dec!(3))
        );
        // 200bps(2%) → 103.02 まで → 全部 = 6.0
        assert_eq!(
            b.max_size_within_slippage(Side::Ask, 200),
            Quantity(dec!(6))
        );
    }

    #[test]
    fn max_size_within_slippage_bid_side() {
        let b = book();
        // best_bid=100。0bps → 100 のみ = 1.0
        assert_eq!(b.max_size_within_slippage(Side::Bid, 0), Quantity(dec!(1)));
        // 100bps → 99 まで → 1.0 + 2.0 = 3.0
        assert_eq!(
            b.max_size_within_slippage(Side::Bid, 100),
            Quantity(dec!(3))
        );
        // 300bps → 97 まで → 全部 = 6.0
        assert_eq!(
            b.max_size_within_slippage(Side::Bid, 300),
            Quantity(dec!(6))
        );
    }

    #[test]
    fn max_size_within_slippage_stops_at_first_gap() {
        // 許容範囲外のレベルが出た時点で打ち切る（その先に近い価格が無いことの確認）
        let b = OrderBook::new(
            Dex::EdgeX,
            Symbol::Sol,
            vec![],
            vec![
                lvl(dec!(100), dec!(1)),
                lvl(dec!(110), dec!(5)),
                lvl(dec!(100.5), dec!(9)), // ソート違反（本来ありえない）でも先に打ち切られる
            ],
            MessageTrace::on_receive(),
        );
        assert_eq!(
            b.max_size_within_slippage(Side::Ask, 100),
            Quantity(dec!(1))
        );
    }

    #[test]
    fn max_size_within_slippage_empty_book_is_zero() {
        assert_eq!(
            empty_book().max_size_within_slippage(Side::Ask, 10),
            Quantity::ZERO
        );
    }

    #[test]
    fn slippage_bps_for_size() {
        let b = book();
        // 買い 2.0 → vwap 101.5 vs best 101 → 0.5/101*10000 ≒ 49.5 bps
        let s = b
            .slippage_bps_for_size(Side::Ask, Quantity(dec!(2)))
            .unwrap();
        assert!((s - dec!(49.5049)).abs() < dec!(0.001), "{s}");
        // 深さ不足なら None
        assert!(b
            .slippage_bps_for_size(Side::Ask, Quantity(dec!(999)))
            .is_none());
    }

    #[test]
    fn validate_accepts_well_formed_book() {
        assert_eq!(book().validate(), Ok(()));
        assert_eq!(empty_book().validate(), Ok(()));
    }

    #[test]
    fn validate_detects_broken_books() {
        let mut b = book();
        b.bids.reverse();
        assert_eq!(b.validate(), Err(BookIntegrityError::BidsNotDescending));

        let mut b = book();
        b.asks.reverse();
        assert_eq!(b.validate(), Err(BookIntegrityError::AsksNotAscending));

        let mut b = book();
        b.asks[0].price = Price(dec!(99));
        assert!(matches!(
            b.validate(),
            Err(BookIntegrityError::Crossed { .. })
        ));

        let mut b = book();
        b.bids[0].quantity = Quantity(dec!(0));
        assert_eq!(b.validate(), Err(BookIntegrityError::NonPositiveQuantity));
    }

    #[test]
    fn is_crossed_detects_bid_above_ask() {
        assert!(!book().is_crossed());
        // 板が片側だけの場合はクロスとは言えない
        assert!(!empty_book().is_crossed());

        let mut b = book();
        b.asks[0].price = Price(dec!(99));
        assert!(b.is_crossed());

        // best_bid == best_ask もクロス扱い（同値で両方向に約定できてしまう）
        let mut b = book();
        b.asks[0].price = Price(dec!(100));
        assert!(b.is_crossed());
    }

    #[test]
    fn validate_allowing_crossed_accepts_crossed_books() {
        // dYdX ではクロスが構造上正常に起こる。板を捨てないための入口。
        let mut b = book();
        b.asks[0].price = Price(dec!(99));
        assert!(matches!(
            b.validate(),
            Err(BookIntegrityError::Crossed { .. })
        ));
        assert_eq!(b.validate_allowing_crossed(), Ok(()));

        // ソート順や数量の異常は許容しない（差分再構築のバグを見逃さないため）
        let mut broken = b.clone();
        broken.bids.reverse();
        assert_eq!(
            broken.validate_allowing_crossed(),
            Err(BookIntegrityError::BidsNotDescending)
        );

        let mut broken = b.clone();
        broken.asks[0].quantity = Quantity(dec!(0));
        assert_eq!(
            broken.validate_allowing_crossed(),
            Err(BookIntegrityError::NonPositiveQuantity)
        );
    }
}
