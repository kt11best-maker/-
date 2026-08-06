//! ファンディング裁定戦略。
//!
//! 同一銘柄について、ファンディングレートが**高い DEX でショート**、
//! **低い（またはマイナスの）DEX でロング**を建てる。両建てなので価格変動リスクは
//! 相殺され、レート差を精算ごとに受け取る。速度要求は低く（数分〜数時間）、
//! 保有時間は時間〜日。執行は指値でよい（[`Urgency::Patient`]）。
//!
//! # 損益式
//!
//! ```text
//! 総損益(bps) = ファンディング差の累計（レート差 × 保有精算回数）
//!             + (エントリー時の価格差 − エグジット時の価格差)   ← ベーシス項
//!             − 手数料（建て往復 + 決済往復 = 両DEX計4回分）
//!             − スリッページ（建て・決済）
//!             − 安全マージン
//! ```
//!
//! # ベーシス項の扱い（重要）
//!
//! perp 同士の両建てでは、価格差（ベーシス）は**利益源ではなく、エントリー・
//! エグジットのコスト（またはボーナス）**として損益に効く。2 DEX 間の価格差が
//! 建てた時と決済した時で変わると、その差分がそのまま損益になる。
//!
//! **既定ではベーシス項をゼロとして評価し、「価格差が不利な時は建てない」という
//! フィルタとしてのみ使う。** 価格差の収束を利益に織り込むと、収束しなかった
//! 場合に想定が崩れるため（`count_favorable_basis = true` で加算もできる）。
//!
//! # 手数料の回収
//!
//! 手数料は建てと決済で両 DEX それぞれに発生する（計 4 回分）。1 精算あたりの
//! レート差が手数料合計を下回る場合、**最低でも何回精算をまたぐ必要があるか**を
//! 計算し（[`core_types::breakeven_intervals`]）、それだけ保有し続けられる前提で
//! なければシグナルを出さない。

use config::FundingArbConfig;
use core_types::{Dex, ExecutionStyle, FundingSpread, Symbol};
use rust_decimal::Decimal;
use strategy_traits::{
    ExitReason, MarketContext, OpenPosition, SignalRationale, Strategy, StrategyKind, TradeSignal,
    Urgency,
};

pub struct FundingArbStrategy {
    cfg: FundingArbConfig,
}

impl FundingArbStrategy {
    pub fn new(cfg: FundingArbConfig) -> Self {
        FundingArbStrategy { cfg }
    }

    pub fn config(&self) -> &FundingArbConfig {
        &self.cfg
    }

    fn evaluate_pair(
        &self,
        ctx: &MarketContext<'_>,
        symbol: Symbol,
        dex_a: Dex,
        dex_b: Dex,
    ) -> Option<TradeSignal> {
        let spread = ctx.funding.spread(symbol, dex_a, dex_b)?;

        // 精算間隔が違う DEX 同士は「1 精算あたりのレート差」が成立しない。
        // 既定では扱わず、扱う場合も年率換算での比較が前提。
        if !spread.same_interval && !self.cfg.allow_mismatched_intervals {
            return None;
        }
        if spread.rate_diff_bps < self.cfg.min_rate_diff_bps {
            return None;
        }

        let fee_bps = ctx.fees.round_trip_bps(
            spread.long_dex,
            spread.short_dex,
            self.cfg.entry_style.into(),
            ExecutionStyle::from(self.cfg.exit_style),
        )?;

        // 手数料を回収できる見込みがあるか
        let breakeven = spread.breakeven_intervals(fee_bps)?;
        if breakeven > self.cfg.max_acceptable_breakeven_intervals {
            return None;
        }
        if breakeven > self.cfg.expected_intervals {
            // 想定保有回数の中で回収できない
            return None;
        }

        // 建てる方向で価格差が有利(正)か不利(負)か
        let basis_entry_bps = ctx.signed_basis_bps(symbol, spread.long_dex, spread.short_dex)?;
        if basis_entry_bps < -self.cfg.max_adverse_basis_bps {
            // 不利なベーシスで建てない。ファンディング差が魅力的でも、
            // 数回分の精算収益が一撃で消えうる。
            return None;
        }

        // 保守評価: 有利なベーシスは既定では利益に加算しない（収束を当てにしない）
        let basis_credit_bps = if self.cfg.count_favorable_basis {
            basis_entry_bps.max(Decimal::ZERO)
        } else {
            Decimal::ZERO
        };

        // 1 精算あたりの純収益。手数料は保有期間全体で 1 回なので按分する。
        let expected_intervals = Decimal::from(self.cfg.expected_intervals);
        let net_per_interval = spread.rate_diff_bps
            + (basis_credit_bps - fee_bps) / expected_intervals
            - self.cfg.safety_buffer_bps;
        if net_per_interval < self.cfg.min_profit_bps {
            return None;
        }

        Some(TradeSignal {
            strategy: StrategyKind::FundingArb,
            symbol,
            long_dex: spread.long_dex,
            short_dex: spread.short_dex,
            notional: self.cfg.notional_usd,
            // 注: これは「1 精算あたり」。価格差戦略の値と直接比較しないこと
            expected_profit_bps: net_per_interval,
            urgency: Urgency::Patient,
            rationale: SignalRationale::FundingArb {
                rate_diff_bps: spread.rate_diff_bps,
                expected_intervals: self.cfg.expected_intervals,
                breakeven_intervals: breakeven,
                basis_entry_bps,
                basis_credit_bps,
                fee_bps,
                buffer_bps: self.cfg.safety_buffer_bps,
                same_interval: spread.same_interval,
            },
            created_at: ctx.now,
            created_at_wall_ms: ctx.now_wall_ms,
        })
    }

    /// 保有中ポジションと同じ向きのレート差を返す。向きが反転していれば `None`。
    fn spread_in_position_direction(
        position: &OpenPosition,
        ctx: &MarketContext<'_>,
    ) -> Option<FundingSpread> {
        let spread = ctx
            .funding
            .spread(position.symbol, position.long_dex, position.short_dex)?;
        (spread.long_dex == position.long_dex && spread.short_dex == position.short_dex)
            .then_some(spread)
    }
}

impl Strategy for FundingArbStrategy {
    fn kind(&self) -> StrategyKind {
        StrategyKind::FundingArb
    }

    fn evaluate(&self, ctx: &MarketContext<'_>) -> Vec<TradeSignal> {
        if !self.cfg.enabled {
            return Vec::new();
        }
        let mut signals = Vec::new();
        for symbol in ctx.symbols {
            for (dex_a, dex_b) in ctx.pairs {
                if let Some(signal) = self.evaluate_pair(ctx, *symbol, *dex_a, *dex_b) {
                    signals.push(signal);
                }
            }
        }
        signals
    }

    /// 解消判定。
    ///
    /// 1. レート差が消滅・反転した → [`ExitReason::FundingEdgeGone`]
    /// 2. レート差が建てる基準を割った → [`ExitReason::FundingBelowCost`]
    /// 3. 最大保有期間に到達 → [`ExitReason::MaxHoldingReached`]
    ///
    /// 証拠金維持率の悪化とキルスイッチはリスク管理層の担当で、ここでは見ない。
    fn should_exit(&self, position: &OpenPosition, ctx: &MarketContext<'_>) -> Option<ExitReason> {
        if position.strategy != StrategyKind::FundingArb {
            return None;
        }

        // 3. 最大保有期間（レートが見えなくても効かせたいので最初に見る）
        if position.holding_hours(ctx.now_wall_ms) >= self.cfg.max_holding_hours {
            return Some(ExitReason::MaxHoldingReached);
        }

        match Self::spread_in_position_direction(position, ctx) {
            // 1. 反転・消滅（レート差そのものが無い場合も含む）
            None => Some(ExitReason::FundingEdgeGone),
            Some(spread) => {
                // 2. 建てる基準を割った = 保有し続ける理由が弱い
                (spread.rate_diff_bps < self.cfg.min_rate_diff_bps)
                    .then_some(ExitReason::FundingBelowCost)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{
        DexFees, FeeSchedule, FundingRate, Level, MessageTrace, OrderBook, Price, Quantity,
    };
    use market_data::{BookStore, FundingStore};
    use rust_decimal_macros::dec;
    use std::time::Instant;

    const NOW_MS: u64 = 1_700_000_000_000;

    fn book(dex: Dex, mid: Decimal) -> OrderBook {
        let mut trace = MessageTrace::on_receive();
        trace.received_wall_ms = NOW_MS;
        OrderBook::new(
            dex,
            Symbol::Btc,
            vec![Level::new(Price(mid), Quantity(dec!(100)))],
            vec![Level::new(Price(mid + dec!(0.01)), Quantity(dec!(100)))],
            trace,
        )
    }

    fn rate(dex: Dex, rate: Decimal, interval_hours: Decimal) -> FundingRate {
        FundingRate {
            dex,
            symbol: Symbol::Btc,
            rate,
            interval_hours,
            next_funding_time_ms: None,
            trace: MessageTrace::on_receive(),
        }
    }

    struct Fixture {
        books: BookStore,
        funding: FundingStore,
        fees: FeeSchedule,
        symbols: Vec<Symbol>,
        pairs: Vec<(Dex, Dex)>,
    }

    impl Fixture {
        /// Hyperliquid のレートが高い（= HL でショート、Lighter でロング）。
        fn new(
            hl_rate: Decimal,
            lighter_rate: Decimal,
            hl_mid: Decimal,
            lighter_mid: Decimal,
        ) -> Self {
            let books = BookStore::new();
            books.update(book(Dex::Hyperliquid, hl_mid));
            books.update(book(Dex::Lighter, lighter_mid));

            let funding = FundingStore::new();
            funding.update(rate(Dex::Hyperliquid, hl_rate, dec!(1)));
            funding.update(rate(Dex::Lighter, lighter_rate, dec!(1)));

            let mut fees = FeeSchedule::new();
            for dex in [Dex::Hyperliquid, Dex::Lighter] {
                fees.insert(
                    dex,
                    DexFees {
                        taker_bps: dec!(2),
                        maker_bps: dec!(1),
                    },
                );
            }
            Fixture {
                books,
                funding,
                fees,
                symbols: vec![Symbol::Btc],
                pairs: vec![(Dex::Hyperliquid, Dex::Lighter)],
            }
        }

        fn ctx(&self) -> MarketContext<'_> {
            MarketContext {
                books: &self.books,
                funding: &self.funding,
                fees: &self.fees,
                symbols: &self.symbols,
                pairs: &self.pairs,
                now_wall_ms: NOW_MS,
                now: Instant::now(),
            }
        }
    }

    fn config() -> FundingArbConfig {
        FundingArbConfig {
            min_rate_diff_bps: dec!(1),
            min_profit_bps: dec!(0.5),
            safety_buffer_bps: dec!(0.5),
            expected_intervals: 12,
            max_acceptable_breakeven_intervals: 6,
            max_adverse_basis_bps: dec!(2),
            count_favorable_basis: false,
            notional_usd: dec!(1000),
            ..Default::default()
        }
    }

    fn position(hours_held: u64) -> OpenPosition {
        OpenPosition {
            strategy: StrategyKind::FundingArb,
            symbol: Symbol::Btc,
            long_dex: Dex::Lighter,
            short_dex: Dex::Hyperliquid,
            notional: dec!(1000),
            entry_basis_bps: Decimal::ZERO,
            entry_rate_diff_bps: dec!(3),
            opened_at_wall_ms: NOW_MS - hours_held * 3_600_000,
            funding_intervals_collected: hours_held as u32,
        }
    }

    #[test]
    fn emits_signal_when_rate_diff_covers_fees() {
        // レート差 3bps/精算、手数料は maker 建て(1+1) + taker 決済(2+2) = 6bps
        // → breakeven 2 回。想定 12 回なので通る
        let f = Fixture::new(dec!(0.0004), dec!(0.0001), dec!(60000), dec!(60000));
        let signals = FundingArbStrategy::new(config()).evaluate(&f.ctx());

        assert_eq!(signals.len(), 1);
        let s = &signals[0];
        assert_eq!(s.strategy, StrategyKind::FundingArb);
        assert_eq!(s.urgency, Urgency::Patient, "指値でじっくり建てる");
        assert_eq!(s.short_dex, Dex::Hyperliquid, "レートが高い方でショート");
        assert_eq!(s.long_dex, Dex::Lighter);

        let SignalRationale::FundingArb {
            rate_diff_bps,
            breakeven_intervals,
            fee_bps,
            basis_credit_bps,
            same_interval,
            ..
        } = &s.rationale
        else {
            panic!("FundingArb の根拠が入っていない");
        };
        assert_eq!(*rate_diff_bps, dec!(3));
        assert_eq!(*fee_bps, dec!(6));
        assert_eq!(*breakeven_intervals, 2);
        assert!(*same_interval);
        assert_eq!(*basis_credit_bps, Decimal::ZERO, "既定では加算しない");

        // 1 精算あたり: 3 - 6/12 - 0.5 = 2.0
        assert_eq!(s.expected_profit_bps, dec!(2.0));
        // 保有期間全体では 24bps。価格差戦略と比較するならこちらを使う
        assert_eq!(s.total_expected_profit_bps(), dec!(24.0));
    }

    #[test]
    fn rejects_when_rate_diff_is_too_small() {
        // 0.5bps < min_rate_diff_bps(1)
        let f = Fixture::new(dec!(0.00015), dec!(0.0001), dec!(60000), dec!(60000));
        assert!(FundingArbStrategy::new(config())
            .evaluate(&f.ctx())
            .is_empty());
    }

    #[test]
    fn rejects_when_breakeven_takes_too_long() {
        // レート差 1bps、手数料 6bps → breakeven 6 回。上限を 3 回にすると弾かれる
        let f = Fixture::new(dec!(0.0002), dec!(0.0001), dec!(60000), dec!(60000));
        let cfg = FundingArbConfig {
            max_acceptable_breakeven_intervals: 3,
            ..config()
        };
        assert!(FundingArbStrategy::new(cfg).evaluate(&f.ctx()).is_empty());

        // 上限 6 回なら通る（ただし利益判定は別途効く）
        let cfg = FundingArbConfig {
            max_acceptable_breakeven_intervals: 6,
            min_profit_bps: dec!(0),
            safety_buffer_bps: dec!(0),
            ..config()
        };
        assert_eq!(FundingArbStrategy::new(cfg).evaluate(&f.ctx()).len(), 1);
    }

    #[test]
    fn rejects_adverse_basis() {
        // ロング側(Lighter)が高い = 建てる方向として不利。
        // 60000 vs 60030 → 約 -5bps で max_adverse_basis_bps(2) を超える
        let f = Fixture::new(dec!(0.0004), dec!(0.0001), dec!(60000), dec!(60030));
        assert!(FundingArbStrategy::new(config())
            .evaluate(&f.ctx())
            .is_empty());

        // 許容幅を広げれば建てられる
        let cfg = FundingArbConfig {
            max_adverse_basis_bps: dec!(10),
            ..config()
        };
        let signals = FundingArbStrategy::new(cfg).evaluate(&f.ctx());
        assert_eq!(signals.len(), 1);
        let SignalRationale::FundingArb {
            basis_entry_bps, ..
        } = &signals[0].rationale
        else {
            panic!()
        };
        assert!(
            *basis_entry_bps < Decimal::ZERO,
            "不利なベーシスが記録される"
        );
    }

    #[test]
    fn favorable_basis_is_not_credited_by_default() {
        // ショート側(HL)が高い = 有利なベーシス
        let f = Fixture::new(dec!(0.0004), dec!(0.0001), dec!(60030), dec!(60000));
        let conservative = FundingArbStrategy::new(config()).evaluate(&f.ctx());
        let SignalRationale::FundingArb {
            basis_entry_bps,
            basis_credit_bps,
            ..
        } = &conservative[0].rationale
        else {
            panic!()
        };
        assert!(*basis_entry_bps > Decimal::ZERO);
        assert_eq!(*basis_credit_bps, Decimal::ZERO, "収束を当てにしない");

        // 明示的に有効化した場合のみ加算される
        let cfg = FundingArbConfig {
            count_favorable_basis: true,
            ..config()
        };
        let optimistic = FundingArbStrategy::new(cfg).evaluate(&f.ctx());
        assert!(optimistic[0].expected_profit_bps > conservative[0].expected_profit_bps);
    }

    #[test]
    fn mismatched_intervals_are_skipped_by_default() {
        let books = BookStore::new();
        books.update(book(Dex::Hyperliquid, dec!(60000)));
        books.update(book(Dex::Aster, dec!(60000)));

        let funding = FundingStore::new();
        funding.update(rate(Dex::Hyperliquid, dec!(0.0001), dec!(1)));
        // Aster は 8 時間精算
        funding.update(rate(Dex::Aster, dec!(0.0010), dec!(8)));

        let mut fees = FeeSchedule::new();
        for dex in [Dex::Hyperliquid, Dex::Aster] {
            fees.insert(
                dex,
                DexFees {
                    taker_bps: dec!(2),
                    maker_bps: dec!(1),
                },
            );
        }
        let f = Fixture {
            books,
            funding,
            fees,
            symbols: vec![Symbol::Btc],
            pairs: vec![(Dex::Hyperliquid, Dex::Aster)],
        };

        assert!(
            FundingArbStrategy::new(config())
                .evaluate(&f.ctx())
                .is_empty(),
            "精算間隔が違うペアは既定で扱わない"
        );

        let cfg = FundingArbConfig {
            allow_mismatched_intervals: true,
            ..config()
        };
        let signals = FundingArbStrategy::new(cfg).evaluate(&f.ctx());
        assert_eq!(signals.len(), 1);
        let SignalRationale::FundingArb { same_interval, .. } = &signals[0].rationale else {
            panic!()
        };
        assert!(!*same_interval, "間隔が違うことが根拠に残る");
    }

    #[test]
    fn missing_fees_block_the_signal() {
        let mut f = Fixture::new(dec!(0.0004), dec!(0.0001), dec!(60000), dec!(60000));
        f.fees = FeeSchedule::new();
        assert!(FundingArbStrategy::new(config())
            .evaluate(&f.ctx())
            .is_empty());
    }

    #[test]
    fn disabled_strategy_emits_nothing() {
        let f = Fixture::new(dec!(0.0004), dec!(0.0001), dec!(60000), dec!(60000));
        let cfg = FundingArbConfig {
            enabled: false,
            ..config()
        };
        assert!(FundingArbStrategy::new(cfg).evaluate(&f.ctx()).is_empty());
    }

    #[test]
    fn exits_when_rate_diff_reverses() {
        let strategy = FundingArbStrategy::new(config());
        // 建てた時と逆（Lighter の方が高くなった）
        let f = Fixture::new(dec!(0.0001), dec!(0.0004), dec!(60000), dec!(60000));
        assert_eq!(
            strategy.should_exit(&position(1), &f.ctx()),
            Some(ExitReason::FundingEdgeGone)
        );
    }

    #[test]
    fn exits_when_rate_diff_falls_below_entry_bar() {
        let strategy = FundingArbStrategy::new(config());
        // 向きは同じだが 0.5bps しかない（min_rate_diff_bps = 1）
        let f = Fixture::new(dec!(0.00015), dec!(0.0001), dec!(60000), dec!(60000));
        assert_eq!(
            strategy.should_exit(&position(1), &f.ctx()),
            Some(ExitReason::FundingBelowCost)
        );
    }

    #[test]
    fn exits_at_max_holding_hours() {
        let strategy = FundingArbStrategy::new(config());
        // レート差は健在でも保有期間で降りる
        let f = Fixture::new(dec!(0.0004), dec!(0.0001), dec!(60000), dec!(60000));
        assert_eq!(strategy.should_exit(&position(71), &f.ctx()), None);
        assert_eq!(
            strategy.should_exit(&position(72), &f.ctx()),
            Some(ExitReason::MaxHoldingReached)
        );
    }

    #[test]
    fn holds_while_edge_persists() {
        let strategy = FundingArbStrategy::new(config());
        let f = Fixture::new(dec!(0.0004), dec!(0.0001), dec!(60000), dec!(60000));
        assert_eq!(strategy.should_exit(&position(5), &f.ctx()), None);
    }

    #[test]
    fn exits_when_rate_data_disappears() {
        let strategy = FundingArbStrategy::new(config());
        let f = Fixture::new(dec!(0.0004), dec!(0.0001), dec!(60000), dec!(60000));
        // レートが同値になり差が消えた状態を作る
        f.funding
            .update(rate(Dex::Hyperliquid, dec!(0.0001), dec!(1)));
        assert_eq!(
            strategy.should_exit(&position(1), &f.ctx()),
            Some(ExitReason::FundingEdgeGone)
        );
    }

    #[test]
    fn ignores_other_strategies_positions() {
        let strategy = FundingArbStrategy::new(config());
        let f = Fixture::new(dec!(0.0001), dec!(0.0004), dec!(60000), dec!(60000));
        let mut p = position(1);
        p.strategy = StrategyKind::PriceArb;
        assert_eq!(strategy.should_exit(&p, &f.ctx()), None);
    }
}
