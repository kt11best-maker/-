//! 価格差アービトラージ戦略。
//!
//! DEX 間の一時的な価格乖離を、両建てで即座に捕まえる。速度要求が極めて高く
//! （数十〜数百 ms）、保有時間は秒〜分。執行は IOC 同時発注（[`Urgency::Immediate`]）。
//!
//! 判定式:
//!
//! ```text
//! 実質乖離 = 生の乖離 − 手数料 − スリッページ − 安全マージン
//! ```
//!
//! - 「生の乖離」は既定で **VWAP ベース**（想定サイズを板に食い込ませた平均約定
//!   価格の差）。この場合スリッページは乖離に織り込み済みなので二重に引かない。
//! - **手数料は既定で 4 レグ**（建て + 決済 × 2 DEX）を引く。捉えた乖離は
//!   いずれ決済するため、建ての 2 レグだけで判定すると過小評価になる
//!   （`count_exit_fees = false` で従来どおりの 2 レグ判定にもできる）。
//! - `staleness_delta_ms` が大きいペアは**判定前に落とす**。片方だけ古い板は
//!   見かけ上の乖離を生むため。

use config::PriceArbConfig;
use core_types::{Dex, ExecutionStyle, Symbol};
use market_data::ExecutableDirection;
use rust_decimal::Decimal;
use strategy_traits::{
    ExitReason, MarketContext, OpenPosition, SignalRationale, Strategy, StrategyKind, TradeSignal,
    Urgency,
};

pub struct PriceArbStrategy {
    cfg: PriceArbConfig,
}

impl PriceArbStrategy {
    pub fn new(cfg: PriceArbConfig) -> Self {
        PriceArbStrategy { cfg }
    }

    pub fn config(&self) -> &PriceArbConfig {
        &self.cfg
    }

    /// 1 ペア分の判定。シグナルにならない場合は `None`。
    fn evaluate_pair(
        &self,
        ctx: &MarketContext<'_>,
        symbol: Symbol,
        dex_a: Dex,
        dex_b: Dex,
    ) -> Option<TradeSignal> {
        let notional = (self.cfg.notional_usd > Decimal::ZERO).then_some(self.cfg.notional_usd);
        let snapshot = ctx
            .books
            .divergence(symbol, dex_a, dex_b, dex_a, notional)?;

        // 片方だけ古い板による見かけ上の乖離を先に落とす
        if snapshot.staleness_delta_ms.abs() > self.cfg.max_staleness_delta_ms {
            return None;
        }

        // クロスした板（dYdX では構造上正常に起こる）から計算した乖離は
        // アービトラージ機会として扱わない。best 気配が実際には取れないため、
        // 存在しない乖離を捉えたことになる。
        if snapshot.has_crossed_book() {
            return None;
        }

        // 執行方向: 高い方で売り（ショート）、安い方で買い（ロング）
        let (short_dex, long_dex) = match snapshot.executable_direction {
            ExecutableDirection::SellABuyB => (snapshot.dex_a, snapshot.dex_b),
            ExecutableDirection::SellBBuyA => (snapshot.dex_b, snapshot.dex_a),
        };

        // VWAP ベースならスリッページは乖離に織り込み済み
        let (gross_spread_bps, slippage_bps, vwap_based) = if self.cfg.use_vwap {
            (snapshot.vwap_spread_bps?, Decimal::ZERO, true)
        } else {
            (
                snapshot.executable_spread_bps,
                self.cfg.assumed_slippage_bps,
                false,
            )
        };

        let fee_bps = self.fee_bps(ctx, long_dex, short_dex)?;
        let net = gross_spread_bps - fee_bps - slippage_bps - self.cfg.safety_buffer_bps;
        if net < self.cfg.min_profit_bps {
            return None;
        }

        Some(TradeSignal {
            strategy: StrategyKind::PriceArb,
            symbol,
            long_dex,
            short_dex,
            notional: self.cfg.notional_usd,
            expected_profit_bps: net,
            urgency: Urgency::Immediate,
            rationale: SignalRationale::PriceArb {
                gross_spread_bps,
                fee_bps,
                slippage_bps,
                buffer_bps: self.cfg.safety_buffer_bps,
                staleness_delta_ms: snapshot.staleness_delta_ms,
                vwap_based,
            },
            created_at: ctx.now,
            created_at_wall_ms: ctx.now_wall_ms,
        })
    }

    /// 手数料。未設定の DEX を含むペアは `None`（= シグナルを出さない）。
    fn fee_bps(&self, ctx: &MarketContext<'_>, long_dex: Dex, short_dex: Dex) -> Option<Decimal> {
        let entry: ExecutionStyle = self.cfg.entry_style.into();
        if self.cfg.count_exit_fees {
            ctx.fees
                .round_trip_bps(long_dex, short_dex, entry, self.cfg.exit_style.into())
        } else {
            ctx.fees.entry_bps(long_dex, short_dex, entry)
        }
    }
}

impl Strategy for PriceArbStrategy {
    fn kind(&self) -> StrategyKind {
        StrategyKind::PriceArb
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

    /// 乖離が消えたら降りる。
    ///
    /// 価格差アービトラージは「乖離が縮むこと」で利益が確定するので、建てた
    /// 方向の優位（ショート側が高い）が無くなった時点が手仕舞い。
    fn should_exit(&self, position: &OpenPosition, ctx: &MarketContext<'_>) -> Option<ExitReason> {
        if position.strategy != StrategyKind::PriceArb {
            return None;
        }
        let current_basis =
            ctx.signed_basis_bps(position.symbol, position.long_dex, position.short_dex)?;
        (current_basis <= Decimal::ZERO).then_some(ExitReason::SpreadConverged)
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

    fn book(dex: Dex, bid: Decimal, ask: Decimal, size: Decimal, wall_ms: u64) -> OrderBook {
        let mut trace = MessageTrace::on_receive();
        trace.received_wall_ms = wall_ms;
        OrderBook::new(
            dex,
            Symbol::Btc,
            vec![Level::new(Price(bid), Quantity(size))],
            vec![Level::new(Price(ask), Quantity(size))],
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
        /// Hyperliquid が安く、Lighter が高い（Lighter で売り・HL で買い）。
        fn new(hl: (Decimal, Decimal), lighter: (Decimal, Decimal), stale_ms: u64) -> Self {
            let books = BookStore::new();
            books.update(book(Dex::Hyperliquid, hl.0, hl.1, dec!(100), 1_000));
            books.update(book(
                Dex::Lighter,
                lighter.0,
                lighter.1,
                dec!(100),
                1_000 + stale_ms,
            ));

            let mut fees = FeeSchedule::new();
            for dex in [Dex::Hyperliquid, Dex::Lighter] {
                fees.insert(
                    dex,
                    DexFees {
                        taker_bps: dec!(1),
                        maker_bps: dec!(0),
                    },
                );
            }
            Fixture {
                books,
                funding: FundingStore::new(),
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
                now_wall_ms: 1_700_000_000_000,
                now: Instant::now(),
            }
        }
    }

    fn config() -> PriceArbConfig {
        PriceArbConfig {
            min_profit_bps: dec!(1),
            safety_buffer_bps: dec!(1),
            notional_usd: dec!(1000),
            max_staleness_delta_ms: 250,
            ..Default::default()
        }
    }

    #[test]
    fn crossed_books_are_excluded_from_signals() {
        // dYdX の板がクロスしている状態。乖離は大きく見えるが実際には取れない。
        let books = BookStore::new();
        books.update(book(
            Dex::Hyperliquid,
            dec!(100),
            dec!(100.1),
            dec!(100),
            1_000,
        ));
        // bid 102 > ask 101 のクロス板
        books.update(book(Dex::Dydx, dec!(102), dec!(101), dec!(100), 1_000));

        let mut fees = FeeSchedule::new();
        for dex in [Dex::Hyperliquid, Dex::Dydx] {
            fees.insert(
                dex,
                DexFees {
                    taker_bps: dec!(1),
                    maker_bps: dec!(0),
                },
            );
        }
        let f = Fixture {
            books,
            funding: FundingStore::new(),
            fees,
            symbols: vec![Symbol::Btc],
            pairs: vec![(Dex::Hyperliquid, Dex::Dydx)],
        };

        // 乖離自体は記録される（フェーズ1 の計測対象）
        let snapshot = f
            .books
            .divergence(Symbol::Btc, Dex::Hyperliquid, Dex::Dydx, Dex::Dydx, None)
            .unwrap();
        assert!(snapshot.book_crossed_b);
        assert!(
            snapshot.executable_spread_bps > Decimal::ZERO,
            "見かけ上は取れる"
        );

        // が、シグナルにはしない
        assert!(PriceArbStrategy::new(config())
            .evaluate(&f.ctx())
            .is_empty());
    }

    #[test]
    fn emits_signal_when_divergence_beats_costs() {
        // HL: 100/100.1、Lighter: 101/101.1 → Lighter で売り HL で買い
        let f = Fixture::new((dec!(100), dec!(100.1)), (dec!(101), dec!(101.1)), 0);
        let strategy = PriceArbStrategy::new(config());
        let signals = strategy.evaluate(&f.ctx());

        assert_eq!(signals.len(), 1);
        let s = &signals[0];
        assert_eq!(s.strategy, StrategyKind::PriceArb);
        assert_eq!(s.urgency, Urgency::Immediate);
        assert_eq!(s.short_dex, Dex::Lighter, "高い方でショート");
        assert_eq!(s.long_dex, Dex::Hyperliquid, "安い方でロング");
        assert!(s.expected_profit_bps > Decimal::ZERO);

        let SignalRationale::PriceArb {
            fee_bps,
            slippage_bps,
            vwap_based,
            ..
        } = &s.rationale
        else {
            panic!("PriceArb の根拠が入っていない");
        };
        // 4 レグ分の手数料（1bps × 2 DEX × 建て/決済）
        assert_eq!(*fee_bps, dec!(4));
        // VWAP ベースならスリッページは二重に引かない
        assert!(*vwap_based);
        assert_eq!(*slippage_bps, Decimal::ZERO);
    }

    #[test]
    fn no_signal_when_costs_exceed_divergence() {
        // 乖離がほぼ無い
        let f = Fixture::new((dec!(100), dec!(100.1)), (dec!(100), dec!(100.1)), 0);
        let strategy = PriceArbStrategy::new(config());
        assert!(strategy.evaluate(&f.ctx()).is_empty());
    }

    #[test]
    fn stale_pairs_are_filtered_out() {
        // 鮮度差 1 秒 → 見かけ上の乖離の疑いが強いので判定しない
        let f = Fixture::new((dec!(100), dec!(100.1)), (dec!(101), dec!(101.1)), 1_000);
        let strategy = PriceArbStrategy::new(config());
        assert!(strategy.evaluate(&f.ctx()).is_empty());

        // 閾値を緩めればシグナルは出る（鮮度差そのものは記録される）
        let lenient = PriceArbConfig {
            max_staleness_delta_ms: 2_000,
            ..config()
        };
        let signals = PriceArbStrategy::new(lenient).evaluate(&f.ctx());
        assert_eq!(signals.len(), 1);
        let SignalRationale::PriceArb {
            staleness_delta_ms, ..
        } = &signals[0].rationale
        else {
            panic!()
        };
        assert_eq!(*staleness_delta_ms, -1_000);
    }

    #[test]
    fn missing_fees_block_the_signal() {
        let mut f = Fixture::new((dec!(100), dec!(100.1)), (dec!(101), dec!(101.1)), 0);
        // Lighter の手数料が未設定 → 判定できないのでシグナルを出さない
        f.fees = {
            let mut s = FeeSchedule::new();
            s.insert(
                Dex::Hyperliquid,
                DexFees {
                    taker_bps: dec!(1),
                    maker_bps: dec!(0),
                },
            );
            s
        };
        assert!(PriceArbStrategy::new(config())
            .evaluate(&f.ctx())
            .is_empty());
    }

    #[test]
    fn exit_fees_can_be_excluded() {
        let f = Fixture::new((dec!(100), dec!(100.1)), (dec!(101), dec!(101.1)), 0);
        let entry_only = PriceArbConfig {
            count_exit_fees: false,
            ..config()
        };
        let signals = PriceArbStrategy::new(entry_only).evaluate(&f.ctx());
        let SignalRationale::PriceArb { fee_bps, .. } = &signals[0].rationale else {
            panic!()
        };
        assert_eq!(*fee_bps, dec!(2), "建ての 2 レグのみ");
    }

    #[test]
    fn best_quote_mode_subtracts_slippage_explicitly() {
        let f = Fixture::new((dec!(100), dec!(100.1)), (dec!(101), dec!(101.1)), 0);
        let cfg = PriceArbConfig {
            use_vwap: false,
            assumed_slippage_bps: dec!(3),
            ..config()
        };
        let signals = PriceArbStrategy::new(cfg).evaluate(&f.ctx());
        let SignalRationale::PriceArb {
            slippage_bps,
            vwap_based,
            ..
        } = &signals[0].rationale
        else {
            panic!()
        };
        assert!(!*vwap_based);
        assert_eq!(*slippage_bps, dec!(3));
    }

    #[test]
    fn thin_book_yields_no_vwap_and_no_signal() {
        // 想定ノーショナル 1000 に対して板が薄すぎる（0.001 単位しかない）
        let books = BookStore::new();
        books.update(book(
            Dex::Hyperliquid,
            dec!(100),
            dec!(100.1),
            dec!(0.001),
            1_000,
        ));
        books.update(book(
            Dex::Lighter,
            dec!(101),
            dec!(101.1),
            dec!(0.001),
            1_000,
        ));
        let mut fees = FeeSchedule::new();
        for dex in [Dex::Hyperliquid, Dex::Lighter] {
            fees.insert(
                dex,
                DexFees {
                    taker_bps: dec!(1),
                    maker_bps: dec!(0),
                },
            );
        }
        let f = Fixture {
            books,
            funding: FundingStore::new(),
            fees,
            symbols: vec![Symbol::Btc],
            pairs: vec![(Dex::Hyperliquid, Dex::Lighter)],
        };
        assert!(PriceArbStrategy::new(config())
            .evaluate(&f.ctx())
            .is_empty());
    }

    #[test]
    fn disabled_strategy_emits_nothing() {
        let f = Fixture::new((dec!(100), dec!(100.1)), (dec!(101), dec!(101.1)), 0);
        let cfg = PriceArbConfig {
            enabled: false,
            ..config()
        };
        assert!(PriceArbStrategy::new(cfg).evaluate(&f.ctx()).is_empty());
    }

    #[test]
    fn exits_when_spread_converges() {
        let strategy = PriceArbStrategy::new(config());
        let position = OpenPosition {
            strategy: StrategyKind::PriceArb,
            symbol: Symbol::Btc,
            long_dex: Dex::Hyperliquid,
            short_dex: Dex::Lighter,
            notional: dec!(1000),
            entry_basis_bps: dec!(100),
            entry_rate_diff_bps: Decimal::ZERO,
            // 価格差アービトラージでは使わない（ファンディング裁定専用）
            breakeven_intervals: 0,
            opened_at_wall_ms: 1_700_000_000_000,
            entry_next_funding_time_ms: None,
            observed_intervals_collected: 0,
            exit_deferred_since_ms: None,
        };

        // まだ乖離が残っている → 保持
        let wide = Fixture::new((dec!(100), dec!(100.1)), (dec!(101), dec!(101.1)), 0);
        assert_eq!(strategy.should_exit(&position, &wide.ctx()), None);

        // 収束した → 手仕舞い
        let converged = Fixture::new((dec!(100), dec!(100.1)), (dec!(100), dec!(100.1)), 0);
        assert_eq!(
            strategy.should_exit(&position, &converged.ctx()),
            Some(ExitReason::SpreadConverged)
        );
    }

    #[test]
    fn ignores_other_strategies_positions() {
        let strategy = PriceArbStrategy::new(config());
        let f = Fixture::new((dec!(100), dec!(100.1)), (dec!(100), dec!(100.1)), 0);
        let position = OpenPosition {
            strategy: StrategyKind::FundingArb,
            symbol: Symbol::Btc,
            long_dex: Dex::Hyperliquid,
            short_dex: Dex::Lighter,
            notional: dec!(1000),
            entry_basis_bps: Decimal::ZERO,
            entry_rate_diff_bps: dec!(2),
            breakeven_intervals: 2,
            opened_at_wall_ms: 1_700_000_000_000,
            entry_next_funding_time_ms: None,
            observed_intervals_collected: 0,
            exit_deferred_since_ms: None,
        };
        assert_eq!(strategy.should_exit(&position, &f.ctx()), None);
    }

    /// 未使用の import を避けるためのダミー（FundingRate は文脈型に含まれる）。
    #[allow(dead_code)]
    fn _unused(_: FundingRate) {}
}
