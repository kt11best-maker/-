use core_types::{
    now_wall_ms, Dex, MessageTrace, OrderBook, Price, Quantity, Side, Symbol, BPS_DENOMINATOR,
};
use rust_decimal::Decimal;

/// 板の厚さ指標を取る際のスリッページ幅（CSV の `depth_*_bps10` に対応）。
pub const DEPTH_SLIPPAGE_BPS: u32 = 10;

/// 実際に約定させる方向。
///
/// 「高い方の DEX で売り、安い方の DEX で買う」の 2 方向を区別する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutableDirection {
    /// A で売り（A の bid を食う）、B で買い（B の ask を食う）。A が割高。
    SellABuyB,
    /// B で売り（B の bid を食う）、A で買い（A の ask を食う）。B が割高。
    SellBBuyA,
}

impl ExecutableDirection {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExecutableDirection::SellABuyB => "sell_a_buy_b",
            ExecutableDirection::SellBBuyA => "sell_b_buy_a",
        }
    }
}

/// 2 つの DEX 間の価格差スナップショット。
#[derive(Debug, Clone)]
pub struct DivergenceSnapshot {
    pub symbol: Symbol,
    pub dex_a: Dex,
    pub dex_b: Dex,

    pub mid_a: Price,
    pub mid_b: Price,
    pub best_bid_a: Price,
    pub best_ask_a: Price,
    pub best_bid_b: Price,
    pub best_ask_b: Price,

    /// mid 価格ベースの乖離（bps）。正なら A が高い。
    pub raw_spread_bps: Decimal,
    /// best bid/ask ベースの「実際に取れる」乖離（bps）。
    /// 2 方向のうち有利な方の値で、板が正常なら通常は負（= スプレッドを払う側）。
    pub executable_spread_bps: Decimal,
    /// `executable_spread_bps` がどちらの方向のものか。
    pub executable_direction: ExecutableDirection,
    /// 想定サイズで約定した場合の VWAP ベース乖離（bps）。
    /// `executable_direction` と同じ方向で計算する。どちらかの板の深さが
    /// 足りない場合は `None`。
    pub vwap_spread_bps: Option<Decimal>,
    /// VWAP 計算に使った数量（想定ノーショナル / 参照 mid）。
    pub vwap_size: Option<Quantity>,

    /// 各 DEX で 10bps 以内に収まる数量（板の厚さ指標）。
    /// 方向に依存しないよう bid/ask の薄い方を採る。
    pub depth_a_bps10: Quantity,
    pub depth_b_bps10: Quantity,

    /// 各 DEX の板がクロス（bid >= ask）していたか。
    ///
    /// **dYdX では構造上正常に起こる**（中央集権的なオーダーブックを持たない
    /// ため）。クロスした板から計算した乖離は実際には取れないので、
    /// アービトラージ機会として扱ってはいけない（[`DivergenceSnapshot::has_crossed_book`]）。
    /// フェーズ1 では発生頻度そのものが計測対象なので、行は捨てずに残す。
    pub book_crossed_a: bool,
    pub book_crossed_b: bool,

    /// 両 DEX の板の鮮度差（A の受信時刻 - B の受信時刻, ms）。
    ///
    /// これが大きいスナップショットは「片方だけ古い」ことによる見かけ上の乖離を
    /// 含む。後の分析で真の乖離と区別するために必ず記録する。
    pub staleness_delta_ms: i64,
    pub computed_at_wall_ms: u64,

    /// この計算のトリガーとなった（= 直前に更新された）DEX。
    pub trigger_dex: Dex,
    pub trace_a: MessageTrace,
    pub trace_b: MessageTrace,
}

impl DivergenceSnapshot {
    /// 2 つの板から価格差を計算する。
    ///
    /// 銘柄が異なる / どちらかの板が片側しか無い場合は `None`。
    /// `vwap_notional` を渡すとそのノーショナル相当の数量で VWAP 乖離も計算する。
    pub fn compute(
        book_a: &OrderBook,
        book_b: &OrderBook,
        trigger_dex: Dex,
        vwap_notional: Option<Decimal>,
    ) -> Option<DivergenceSnapshot> {
        if book_a.symbol != book_b.symbol || book_a.dex == book_b.dex {
            return None;
        }

        let mid_a = book_a.mid()?;
        let mid_b = book_b.mid()?;
        let best_bid_a = book_a.best_bid()?;
        let best_ask_a = book_a.best_ask()?;
        let best_bid_b = book_b.best_bid()?;
        let best_ask_b = book_b.best_ask()?;

        // 基準価格は両 mid の中点。A/B を入れ替えても絶対値が変わらないようにする。
        let reference = (mid_a.0 + mid_b.0) / Decimal::TWO;
        if reference <= Decimal::ZERO {
            return None;
        }
        let to_bps = |diff: Decimal| diff / reference * BPS_DENOMINATOR;

        let raw_spread_bps = to_bps(mid_a.0 - mid_b.0);

        // A で売り × B で買い / B で売り × A で買い の両方向を評価する。
        let sell_a_buy_b = to_bps(best_bid_a.0 - best_ask_b.0);
        let sell_b_buy_a = to_bps(best_bid_b.0 - best_ask_a.0);
        let (executable_spread_bps, executable_direction) = if sell_a_buy_b >= sell_b_buy_a {
            (sell_a_buy_b, ExecutableDirection::SellABuyB)
        } else {
            (sell_b_buy_a, ExecutableDirection::SellBBuyA)
        };

        let vwap_size = vwap_notional
            .filter(|n| *n > Decimal::ZERO)
            .map(|n| Quantity(n / reference));
        let vwap_spread_bps = vwap_size.and_then(|size| {
            let (sell_book, buy_book) = match executable_direction {
                ExecutableDirection::SellABuyB => (book_a, book_b),
                ExecutableDirection::SellBBuyA => (book_b, book_a),
            };
            let sell_vwap = sell_book.vwap_for_size(Side::Bid, size)?;
            let buy_vwap = buy_book.vwap_for_size(Side::Ask, size)?;
            Some(to_bps(sell_vwap.0 - buy_vwap.0))
        });

        let depth_a_bps10 = thinner_side_depth(book_a);
        let depth_b_bps10 = thinner_side_depth(book_b);

        let staleness_delta_ms =
            book_a.trace.received_wall_ms as i64 - book_b.trace.received_wall_ms as i64;

        // 判定完了時刻はトリガー側の trace にのみ刻む。非トリガー側は受信からの
        // 待ち時間を含んでしまい、内部処理時間の指標にならないため。
        let mut trace_a = book_a.trace;
        let mut trace_b = book_b.trace;
        match trigger_dex {
            d if d == book_a.dex => trace_a.mark_evaluated(),
            d if d == book_b.dex => trace_b.mark_evaluated(),
            _ => {}
        }

        Some(DivergenceSnapshot {
            symbol: book_a.symbol,
            dex_a: book_a.dex,
            dex_b: book_b.dex,
            mid_a,
            mid_b,
            best_bid_a,
            best_ask_a,
            best_bid_b,
            best_ask_b,
            raw_spread_bps,
            executable_spread_bps,
            executable_direction,
            vwap_spread_bps,
            vwap_size,
            depth_a_bps10,
            depth_b_bps10,
            book_crossed_a: book_a.is_crossed(),
            book_crossed_b: book_b.is_crossed(),
            staleness_delta_ms,
            computed_at_wall_ms: now_wall_ms(),
            trigger_dex,
            trace_a,
            trace_b,
        })
    }

    /// どちらかの板がクロスしていたか。
    ///
    /// **true のスナップショットをアービトラージ機会として扱ってはいけない。**
    /// クロスした板の best 気配は実際には取れない（取れるならすでに誰かが
    /// 取っている）ので、乖離が実在するように見えてしまう。
    pub fn has_crossed_book(&self) -> bool {
        self.book_crossed_a || self.book_crossed_b
    }

    /// トリガーとなった側の内部処理時間（受信 → 判定完了）。
    pub fn pipeline_latency_us(&self) -> Option<u64> {
        let trace = if self.trigger_dex == self.dex_a {
            &self.trace_a
        } else {
            &self.trace_b
        };
        trace
            .total_pipeline_latency()
            .map(|d| d.as_micros().min(u64::MAX as u128) as u64)
    }

    /// 取引所 → 受信の遅延（wall clock, ms）。
    pub fn exchange_latency_ms(&self, dex: Dex) -> Option<i64> {
        if dex == self.dex_a {
            self.trace_a.exchange_to_local_ms()
        } else if dex == self.dex_b {
            self.trace_b.exchange_to_local_ms()
        } else {
            None
        }
    }
}

/// bid/ask のうち薄い方の 10bps 深さ。
///
/// アービトラージでは片方の DEX で買い、もう片方で売るため、どちらのサイドを
/// 使うかは方向次第。方向に依存しない保守的な指標として薄い方を採る。
fn thinner_side_depth(book: &OrderBook) -> Quantity {
    let bid = book.max_size_within_slippage(Side::Bid, DEPTH_SLIPPAGE_BPS);
    let ask = book.max_size_within_slippage(Side::Ask, DEPTH_SLIPPAGE_BPS);
    if bid.0 <= ask.0 {
        bid
    } else {
        ask
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{Level, MessageTrace};
    use rust_decimal_macros::dec;

    fn lvl(p: Decimal, q: Decimal) -> Level {
        Level::new(Price(p), Quantity(q))
    }

    fn book(dex: Dex, bids: Vec<Level>, asks: Vec<Level>, wall_ms: u64) -> OrderBook {
        let mut trace = MessageTrace::on_receive();
        trace.received_wall_ms = wall_ms;
        trace.mark_normalized();
        OrderBook::new(dex, Symbol::Btc, bids, asks, trace)
    }

    /// A: bid 100 / ask 101, B: bid 100 / ask 101（完全一致）
    fn flat_pair() -> (OrderBook, OrderBook) {
        let a = book(
            Dex::Hyperliquid,
            vec![lvl(dec!(100), dec!(5))],
            vec![lvl(dec!(101), dec!(5))],
            1_000,
        );
        let b = book(
            Dex::EdgeX,
            vec![lvl(dec!(100), dec!(5))],
            vec![lvl(dec!(101), dec!(5))],
            1_000,
        );
        (a, b)
    }

    #[test]
    fn identical_books_have_zero_raw_spread() {
        let (a, b) = flat_pair();
        let s = DivergenceSnapshot::compute(&a, &b, Dex::Hyperliquid, None).unwrap();
        assert_eq!(s.raw_spread_bps, Decimal::ZERO);
        // 取れる方向はどちらもスプレッド分だけマイナス: (100-101)/100.5*10000
        assert!((s.executable_spread_bps - dec!(-99.5024)).abs() < dec!(0.001));
        assert_eq!(s.staleness_delta_ms, 0);
    }

    #[test]
    fn raw_spread_sign_shows_which_dex_is_richer() {
        // A の mid が高い
        let a = book(
            Dex::Hyperliquid,
            vec![lvl(dec!(110), dec!(5))],
            vec![lvl(dec!(111), dec!(5))],
            1_000,
        );
        let (_, b) = flat_pair();
        let s = DivergenceSnapshot::compute(&a, &b, Dex::Hyperliquid, None).unwrap();
        assert!(s.raw_spread_bps > Decimal::ZERO, "{}", s.raw_spread_bps);

        // A/B を入れ替えると符号だけ反転する
        let flipped = DivergenceSnapshot::compute(&b, &a, Dex::EdgeX, None).unwrap();
        assert_eq!(flipped.raw_spread_bps, -s.raw_spread_bps);
    }

    #[test]
    fn executable_spread_picks_profitable_direction() {
        // A: bid 110 / ask 111、B: bid 100 / ask 101 → A で売り B で買い
        let a = book(
            Dex::Hyperliquid,
            vec![lvl(dec!(110), dec!(5))],
            vec![lvl(dec!(111), dec!(5))],
            1_000,
        );
        let (_, b) = flat_pair();
        let s = DivergenceSnapshot::compute(&a, &b, Dex::Hyperliquid, None).unwrap();
        assert_eq!(s.executable_direction, ExecutableDirection::SellABuyB);
        // (110 - 101) / 105.5 * 10000 ≒ 853 bps
        assert!(
            (s.executable_spread_bps - dec!(853.0805)).abs() < dec!(0.01),
            "{}",
            s.executable_spread_bps
        );

        // 逆向き（B が割高）
        let s2 = DivergenceSnapshot::compute(&b, &a, Dex::EdgeX, None).unwrap();
        assert_eq!(s2.executable_direction, ExecutableDirection::SellBBuyA);
        assert_eq!(s2.executable_spread_bps, s.executable_spread_bps);
    }

    #[test]
    fn vwap_spread_uses_deeper_levels_and_is_worse_than_top_of_book() {
        // A の bid を階段状にして、サイズを増やすと不利になることを見る
        let a = book(
            Dex::Hyperliquid,
            vec![lvl(dec!(110), dec!(1)), lvl(dec!(105), dec!(100))],
            vec![lvl(dec!(111), dec!(100))],
            1_000,
        );
        let b = book(
            Dex::EdgeX,
            vec![lvl(dec!(100), dec!(100))],
            vec![lvl(dec!(101), dec!(100))],
            1_000,
        );
        // 参照 mid ≒ 105.5 → ノーショナル 1055 で size ≒ 10
        let s = DivergenceSnapshot::compute(&a, &b, Dex::Hyperliquid, Some(dec!(1055))).unwrap();
        let vwap = s.vwap_spread_bps.unwrap();
        assert!(
            vwap < s.executable_spread_bps,
            "vwap={vwap} exec={}",
            s.executable_spread_bps
        );
    }

    #[test]
    fn vwap_spread_is_none_when_book_too_thin() {
        let (a, b) = flat_pair();
        // 両板とも 5 単位しかないので巨大ノーショナルでは深さ不足
        let s =
            DivergenceSnapshot::compute(&a, &b, Dex::Hyperliquid, Some(dec!(1_000_000))).unwrap();
        assert!(s.vwap_spread_bps.is_none());
        assert!(s.vwap_size.is_some());
    }

    #[test]
    fn vwap_is_none_without_notional() {
        let (a, b) = flat_pair();
        let s = DivergenceSnapshot::compute(&a, &b, Dex::Hyperliquid, None).unwrap();
        assert!(s.vwap_spread_bps.is_none());
        assert!(s.vwap_size.is_none());
    }

    #[test]
    fn staleness_delta_records_freshness_gap() {
        let a = book(
            Dex::Hyperliquid,
            vec![lvl(dec!(100), dec!(5))],
            vec![lvl(dec!(101), dec!(5))],
            5_000,
        );
        let b = book(
            Dex::EdgeX,
            vec![lvl(dec!(100), dec!(5))],
            vec![lvl(dec!(101), dec!(5))],
            3_800,
        );
        let s = DivergenceSnapshot::compute(&a, &b, Dex::Hyperliquid, None).unwrap();
        assert_eq!(s.staleness_delta_ms, 1_200);
    }

    #[test]
    fn depth_uses_thinner_side() {
        // bid 側が薄い板
        let a = book(
            Dex::Hyperliquid,
            vec![lvl(dec!(100), dec!(2))],
            vec![lvl(dec!(101), dec!(50))],
            1_000,
        );
        let (_, b) = flat_pair();
        let s = DivergenceSnapshot::compute(&a, &b, Dex::Hyperliquid, None).unwrap();
        assert_eq!(s.depth_a_bps10, Quantity(dec!(2)));
        assert_eq!(s.depth_b_bps10, Quantity(dec!(5)));
    }

    #[test]
    fn pipeline_latency_recorded_only_for_trigger_side() {
        let (a, b) = flat_pair();
        let s = DivergenceSnapshot::compute(&a, &b, Dex::Hyperliquid, None).unwrap();
        assert!(s.trace_a.evaluated_instant.is_some());
        assert!(s.trace_b.evaluated_instant.is_none());
        assert!(s.pipeline_latency_us().is_some());
    }

    #[test]
    fn rejects_mismatched_inputs() {
        let (a, _) = flat_pair();
        let other_symbol = OrderBook::new(
            Dex::EdgeX,
            Symbol::Eth,
            vec![lvl(dec!(100), dec!(5))],
            vec![lvl(dec!(101), dec!(5))],
            MessageTrace::on_receive(),
        );
        assert!(DivergenceSnapshot::compute(&a, &other_symbol, Dex::EdgeX, None).is_none());
        // 同一 DEX 同士は比較しない
        assert!(DivergenceSnapshot::compute(&a, &a, Dex::Hyperliquid, None).is_none());
    }

    #[test]
    fn crossed_books_are_flagged_but_still_recorded() {
        // dYdX ではクロスが構造上正常に起こる。行は残しつつフラグで区別する。
        let crossed = book(
            Dex::Dydx,
            vec![lvl(dec!(102), dec!(5))],
            vec![lvl(dec!(101), dec!(5))],
            1_000,
        );
        let (a, _) = flat_pair();

        let s = DivergenceSnapshot::compute(&a, &crossed, Dex::Dydx, None).unwrap();
        assert!(!s.book_crossed_a);
        assert!(s.book_crossed_b);
        assert!(s.has_crossed_book());

        // 正常な板同士ならフラグは立たない
        let (a, b) = flat_pair();
        let s = DivergenceSnapshot::compute(&a, &b, Dex::Hyperliquid, None).unwrap();
        assert!(!s.has_crossed_book());
    }

    #[test]
    fn one_sided_book_yields_no_snapshot() {
        let (_, b) = flat_pair();
        let one_sided = book(
            Dex::Hyperliquid,
            vec![lvl(dec!(100), dec!(5))],
            vec![],
            1_000,
        );
        assert!(DivergenceSnapshot::compute(&one_sided, &b, Dex::Hyperliquid, None).is_none());
    }
}
