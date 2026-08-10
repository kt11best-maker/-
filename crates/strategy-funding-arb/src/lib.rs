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
use tracing::{debug, warn};

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

        // ベーシス判定の前に板の鮮度を確認する。片方だけ古い板では、実在しない
        // 不利ベーシスで機会を逃すか、実在する不利ベーシスを見逃す。
        // 判定できない以上、安全側に倒してシグナルを出さない。
        match ctx.staleness_delta_ms(symbol, spread.long_dex, spread.short_dex) {
            Some(delta) if delta.abs() > self.cfg.max_staleness_delta_ms => {
                // フェーズ1〜2 の分析で「この理由でどれだけ機会を落としたか」を
                // 追えるようにログに残す。
                debug!(
                    strategy = %StrategyKind::FundingArb,
                    symbol = %symbol,
                    long_dex = %spread.long_dex,
                    short_dex = %spread.short_dex,
                    staleness_delta_ms = delta,
                    threshold_ms = self.cfg.max_staleness_delta_ms,
                    rate_diff_bps = %spread.rate_diff_bps,
                    "板の鮮度差が大きいためファンディング裁定の判定をスキップ"
                );
                return None;
            }
            // 板が揃っていなければベーシスも取れない（下の ? で弾かれる）
            _ => {}
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

    /// 両 DEX のファンディングデータが揃っているか。
    ///
    /// レート差の消滅とデータ欠損は原因が違う（後者は API 不調・WS 切断の
    /// 疑い）。[`ExitReason`] を分けるためにここで区別する。
    fn funding_data_available(position: &OpenPosition, ctx: &MarketContext<'_>) -> bool {
        ctx.funding
            .get(position.long_dex, position.symbol)
            .is_some()
            && ctx
                .funding
                .get(position.short_dex, position.symbol)
                .is_some()
    }

    /// このポジションの精算間隔（時間）。
    ///
    /// 両 DEX で異なる場合は**長い方**を使う。両方の精算を跨がないとレート差を
    /// 完全には取れないため、短い方で数えると回収したつもりで回収できていない。
    /// どちらも不明なら `None`。
    fn position_interval_hours(
        position: &OpenPosition,
        ctx: &MarketContext<'_>,
    ) -> Option<Decimal> {
        let long = ctx
            .funding
            .get(position.long_dex, position.symbol)
            .and_then(|r| r.interval_hours);
        let short = ctx
            .funding
            .get(position.short_dex, position.symbol)
            .and_then(|r| r.interval_hours);
        match (long, short) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(v), None) | (None, Some(v)) => Some(v),
            (None, None) => None,
        }
    }

    /// 手数料を回収し終えたか。
    ///
    /// 回収回数は**時刻から導出する**（[`OpenPosition::intervals_collected`]）。
    /// 執行レイヤーがフィールドを更新してくれる前提にすると、更新漏れで
    /// 「回収 0 回のまま `max_holding_hours` まで抱え続ける」事故が静かに起きる。
    ///
    /// 精算間隔が不明な場合は回数を数えられないので `true`（= 回収済み扱い）を
    /// 返す。検証できない前提で無期限に持ち続ける方が危険なため。
    fn fees_recovered(&self, position: &OpenPosition, ctx: &MarketContext<'_>) -> bool {
        let Some(interval_hours) = Self::position_interval_hours(position, ctx) else {
            debug!(
                strategy = %StrategyKind::FundingArb,
                symbol = %position.symbol,
                "精算間隔が不明なため回収回数を数えられません。回収済みとして扱います"
            );
            return true;
        };

        let derived = position.intervals_collected(ctx.now_wall_ms, interval_hours);
        Self::warn_on_interval_mismatch(position, derived);
        derived >= position.breakeven_intervals
    }

    /// 導出値と執行レイヤーの観測値が乖離していたら警告する。
    ///
    /// 精算の検知漏れかレートデータの異常を示唆する。差が 1 以内なら精算
    /// タイミングの境界による誤差として許容する。
    fn warn_on_interval_mismatch(position: &OpenPosition, derived: u32) {
        if position.observed_intervals_collected == 0 {
            // 執行レイヤーがまだ観測を入れていない（フェーズ1〜2）
            return;
        }
        if derived.abs_diff(position.observed_intervals_collected) > 1 {
            warn!(
                strategy = %StrategyKind::FundingArb,
                symbol = %position.symbol,
                derived,
                observed = position.observed_intervals_collected,
                "精算回数の導出値と観測値が乖離している"
            );
        }
    }

    /// ベーシスが不利な間、このエグジットを保留してよいか。
    ///
    /// | 理由 | 緊急度 | 保留 |
    /// |---|---|---|
    /// | [`ExitReason::FundingEdgeGone`]（反転） | 高 | 不可。保有理由が消えている |
    /// | [`ExitReason::FundingDataUnavailable`] | 高 | 不可。API 不調の疑いがあり状況が悪化しうる |
    /// | [`ExitReason::MaxHoldingReached`] | 中 | 可（保留上限つき） |
    /// | [`ExitReason::FundingBelowCost`] | 低 | 可。有利なベーシスを待つ価値がある |
    ///
    /// リスク管理層由来（証拠金・キルスイッチ）はそもそもここの管轄外。
    fn is_deferrable(reason: ExitReason) -> bool {
        matches!(
            reason,
            ExitReason::FundingBelowCost | ExitReason::MaxHoldingReached
        )
    }

    /// ベーシス・保留上限を考慮しない素のエグジット理由。
    fn raw_exit_reason(
        &self,
        position: &OpenPosition,
        ctx: &MarketContext<'_>,
    ) -> Option<ExitReason> {
        // 1. 最大保有期間（レートが見えなくても効かせたいので最初に見る）
        if position.holding_hours(ctx.now_wall_ms) >= self.cfg.max_holding_hours {
            return Some(ExitReason::MaxHoldingReached);
        }

        // 2. データ欠損。レート差の消滅とは原因が違うので理由を分ける
        if !Self::funding_data_available(position, ctx) {
            return Some(ExitReason::FundingDataUnavailable);
        }

        match Self::spread_in_position_direction(position, ctx) {
            // 3. 反転・消滅 → breakeven 未達でも即降りる
            None => Some(ExitReason::FundingEdgeGone),
            Some(spread) => {
                // 4a. 手数料が未回収の間は、向きが同じなら保有を続ける。
                //     ここで降りると払った手数料がそのまま損になる。
                if !self.fees_recovered(position, ctx) {
                    return None;
                }
                // 4b. 回収後は、エントリーより低い独立した閾値で判定する
                (spread.rate_diff_bps < self.cfg.exit_rate_diff_bps)
                    .then_some(ExitReason::FundingBelowCost)
            }
        }
    }

    /// 決済方向のベーシスが不利すぎるので保留すべきか。
    ///
    /// **鮮度が判定できない場合は保留しない。** エントリーとは安全側の向きが
    /// 逆である点に注意: エントリーは「判定できないなら建てない」が安全だが、
    /// エグジットは「判定できないなら待たない」が安全（不確かなデータを根拠に
    /// 持ち続ける方が危険）。
    fn should_defer_exit(
        &self,
        position: &OpenPosition,
        ctx: &MarketContext<'_>,
        reason: ExitReason,
    ) -> bool {
        // 保留の上限を超えていたら、ベーシスがどうあれ降りる
        if position.exit_deferred_hours(ctx.now_wall_ms) >= self.cfg.max_exit_deferral_hours {
            return false;
        }

        // 鮮度が判定できない（板が揃っていない）なら保留しない
        let Some(delta) =
            ctx.staleness_delta_ms(position.symbol, position.long_dex, position.short_dex)
        else {
            return false;
        };
        if delta.abs() > self.cfg.max_staleness_delta_ms {
            return false;
        }

        // 板は揃っているが mid が取れない（片側だけの板）なら保留しない
        let Some(exit_basis_bps) = ctx.signed_exit_basis_bps(position) else {
            return false;
        };
        if exit_basis_bps >= -self.cfg.max_adverse_exit_basis_bps {
            return false;
        }

        debug!(
            strategy = %StrategyKind::FundingArb,
            symbol = %position.symbol,
            reason = reason.as_str(),
            exit_basis_bps = %exit_basis_bps,
            threshold_bps = %self.cfg.max_adverse_exit_basis_bps,
            deferred_hours = %position.exit_deferred_hours(ctx.now_wall_ms),
            "決済方向のベーシスが不利なためエグジットを保留"
        );
        true
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
    /// # エントリーとエグジットは同じ基準で判断してはいけない
    ///
    /// エントリーは手数料を `expected_intervals` で按分して採算を見る。つまり
    /// 「その回数だけ精算をまたぐ」前提で建てている。にもかかわらず、エントリー
    /// 基準（`min_rate_diff_bps`）を割った瞬間に降りると、**手数料を回収し切る
    /// 前に確定損を出す**。
    ///
    /// 例: レート差 3bps/精算・手数料 6bps（breakeven 2 回）で建て、1 回精算した
    /// 時点でレート差が 0.9bps に細って降りた場合 →
    /// 収益 3bps − 手数料 6bps = **−3bps の確定損**。
    ///
    /// **すでに払った手数料はサンクコスト**なので、降りる判断は「エントリー基準を
    /// 割ったか」ではなく「**今後の期待収益 vs 今降りるコスト**」で行う。
    ///
    /// # 判定の 4 系統
    ///
    /// 1. 最大保有期間に到達 → [`ExitReason::MaxHoldingReached`]
    ///    （レートが見えなくても効かせたいので最初に見る）
    /// 2. データ欠損 → [`ExitReason::FundingDataUnavailable`]
    /// 3. **反転**（保有と逆向きになった / レート差が消滅） →
    ///    [`ExitReason::FundingEdgeGone`]。保有し続ける理由が消えているので
    ///    **breakeven 未達でも即降りる**
    /// 4. **細っただけ** → breakeven を回収するまでは保有継続。回収後は
    ///    `exit_rate_diff_bps`（エントリーより**低い**独立した閾値）を下回ったら
    ///    [`ExitReason::FundingBelowCost`]
    ///
    /// `min_rate_diff_bps` と `exit_rate_diff_bps` の差がヒステリシス帯になり、
    /// 閾値付近でレート差が振動しても建て直しを繰り返さない。
    ///
    /// 回収回数は**時刻から導出する**。執行レイヤーがフィールドを更新して
    /// くれる前提にすると、更新漏れで「回収 0 回のまま `max_holding_hours` まで
    /// 抱え続ける」事故が静かに起きるため。
    ///
    /// # ベーシスによる保留
    ///
    /// 緊急性の低いエグジット（`FundingBelowCost` / `MaxHoldingReached`）は、
    /// **決済方向のベーシスが不利な間は保留**する。ファンディング裁定は
    /// [`Urgency::Patient`] なのだから、「レート差は細ったが今はベーシスが
    /// 不利なので有利に戻るまで待つ」という選択ができる。反転・データ欠損は
    /// 緊急性が高いので保留しない。
    ///
    /// 保留状態（`exit_deferred_since_ms`）の更新は呼び出し側の責務。ここは
    /// `&self` しか持たないので判定のみを行う。
    ///
    /// 証拠金維持率の悪化とキルスイッチはリスク管理層の担当で、ここでは見ない。
    fn should_exit(&self, position: &OpenPosition, ctx: &MarketContext<'_>) -> Option<ExitReason> {
        if position.strategy != StrategyKind::FundingArb {
            return None;
        }

        let reason = self.raw_exit_reason(position, ctx)?;

        // 緊急度が高い理由はベーシスを見ずに即降りる
        if !Self::is_deferrable(reason) {
            return Some(reason);
        }
        if self.should_defer_exit(position, ctx, reason) {
            return None;
        }
        Some(reason)
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

    fn rate(dex: Dex, current_rate: Decimal, interval_hours: Decimal) -> FundingRate {
        FundingRate {
            dex,
            symbol: Symbol::Btc,
            current_rate,
            predicted_rate: None,
            interval_hours: Some(interval_hours),
            next_funding_time_ms: None,
            index_price: None,
            mark_price: None,
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
            // エントリーより低い独立した閾値（ヒステリシス帯 = 0.3〜1.0bps）
            exit_rate_diff_bps: dec!(0.3),
            max_staleness_delta_ms: 1_000,
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

    /// breakeven 2 回のポジション（`emits_signal_when_rate_diff_covers_fees` と
    /// 同じ数値）。
    ///
    /// 1 時間精算・エントリーの 1 時間後が初回精算なので、
    /// **保有時間（時間）= 回収済み精算回数**になる。
    fn position(hours_held: u64) -> OpenPosition {
        position_with(hours_held, 2)
    }

    /// `hours_held` 時間前に建てたポジション。
    ///
    /// **回収回数はフィールドで渡さない**（時刻から導出される）。ここが
    /// 修正の要点で、執行レイヤーの更新漏れで判定が壊れないようにしている。
    fn position_with(hours_held: u64, breakeven_intervals: u32) -> OpenPosition {
        let opened_at_wall_ms = NOW_MS - hours_held * 3_600_000;
        OpenPosition {
            strategy: StrategyKind::FundingArb,
            symbol: Symbol::Btc,
            long_dex: Dex::Lighter,
            short_dex: Dex::Hyperliquid,
            notional: dec!(1000),
            entry_basis_bps: Decimal::ZERO,
            entry_rate_diff_bps: dec!(3),
            breakeven_intervals,
            opened_at_wall_ms,
            // 建てた 1 時間後が初回精算
            entry_next_funding_time_ms: Some(opened_at_wall_ms + 3_600_000),
            observed_intervals_collected: 0,
            exit_deferred_since_ms: None,
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
    fn stale_books_block_the_signal() {
        // 片方の板だけが古いと、ベーシスのフィルタが実在しない価格差を見る
        // ことになる。判定できない以上、安全側に倒してシグナルを出さない。
        let books = BookStore::new();
        books.update(book(Dex::Hyperliquid, dec!(60000)));

        // Lighter の板を 1.5 秒古くする（max_staleness_delta_ms = 1000）
        let mut stale = book(Dex::Lighter, dec!(60000));
        stale.trace.received_wall_ms = NOW_MS - 1_500;
        books.update(stale);

        let funding = FundingStore::new();
        funding.update(rate(Dex::Hyperliquid, dec!(0.0004), dec!(1)));
        funding.update(rate(Dex::Lighter, dec!(0.0001), dec!(1)));

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
        let f = Fixture {
            books,
            funding,
            fees,
            symbols: vec![Symbol::Btc],
            pairs: vec![(Dex::Hyperliquid, Dex::Lighter)],
        };

        assert!(
            FundingArbStrategy::new(config())
                .evaluate(&f.ctx())
                .is_empty(),
            "鮮度差 1500ms > 閾値 1000ms"
        );

        // 閾値を緩めれば同じ市場状態でもシグナルが出る
        let cfg = FundingArbConfig {
            max_staleness_delta_ms: 2_000,
            ..config()
        };
        assert_eq!(FundingArbStrategy::new(cfg).evaluate(&f.ctx()).len(), 1);
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
    fn exits_when_rate_diff_reverses_even_before_breakeven() {
        let strategy = FundingArbStrategy::new(config());
        // 建てた時と逆（Lighter の方が高くなった）
        let f = Fixture::new(dec!(0.0001), dec!(0.0004), dec!(60000), dec!(60000));
        // 回収 0 回でも即降りる。保有し続ける理由そのものが消えているため
        assert_eq!(
            strategy.should_exit(&position_with(0, 2), &f.ctx()),
            Some(ExitReason::FundingEdgeGone)
        );
        assert_eq!(
            strategy.should_exit(&position_with(5, 2), &f.ctx()),
            Some(ExitReason::FundingEdgeGone)
        );
    }

    #[test]
    fn holds_through_thinning_until_fees_are_recovered() {
        // 修正の核心。レート差 3bps・手数料 6bps（breakeven 2 回）で建てた後、
        // 1 回精算した時点でレート差が細っても降りてはいけない。
        // ここで降りると 収益 3bps − 手数料 6bps = −3bps の確定損になる。
        let strategy = FundingArbStrategy::new(config());
        // 向きは同じだが 0.1bps しかない（exit_rate_diff_bps = 0.3 も下回る）
        let f = Fixture::new(dec!(0.00011), dec!(0.0001), dec!(60000), dec!(60000));

        assert_eq!(
            strategy.should_exit(&position_with(0, 2), &f.ctx()),
            None,
            "回収 0/2 回では降りない"
        );
        assert_eq!(
            strategy.should_exit(&position_with(1, 2), &f.ctx()),
            None,
            "回収 1/2 回でも降りない（手数料はサンクコスト）"
        );
        // 回収し終えて初めて exit 閾値で判定される
        assert_eq!(
            strategy.should_exit(&position_with(2, 2), &f.ctx()),
            Some(ExitReason::FundingBelowCost)
        );
    }

    #[test]
    fn recovery_is_derived_from_time_not_from_a_tracked_field() {
        // 修正4 の回帰テスト。observed_intervals_collected を誰も更新しなくても、
        // 時間経過だけで FundingBelowCost に到達すること。
        // フィールド追跡に頼ると「更新漏れ → 常に 0 → max_holding_hours まで
        // 抱え続ける」という機能不全が静かに起きる。
        let strategy = FundingArbStrategy::new(config());
        let f = Fixture::new(dec!(0.00011), dec!(0.0001), dec!(60000), dec!(60000));

        let mut p = position_with(3, 2);
        p.observed_intervals_collected = 0; // 執行レイヤーが一度も更新していない
        assert_eq!(
            strategy.should_exit(&p, &f.ctx()),
            Some(ExitReason::FundingBelowCost),
            "フィールドが 0 のままでも時間経過で回収済みと判断される"
        );
    }

    #[test]
    fn intervals_are_counted_from_the_first_settlement_time() {
        let hourly = dec!(1);
        // NOW の 90 分前に建て、初回精算は建てた 30 分後（= NOW の 60 分前）
        let mut p = position_with(0, 2);
        p.opened_at_wall_ms = NOW_MS - 90 * 60_000;
        p.entry_next_funding_time_ms = Some(NOW_MS - 60 * 60_000);

        // 初回精算の直前は 0
        assert_eq!(
            p.intervals_collected(NOW_MS - 61 * 60_000, hourly),
            0,
            "跨ぐ前は 0（エントリー直後に breakeven 判定が通ってはいけない）"
        );
        // 跨いだ瞬間に 1
        assert_eq!(p.intervals_collected(NOW_MS - 60 * 60_000, hourly), 1);
        // さらに 1 時間で 2
        assert_eq!(p.intervals_collected(NOW_MS, hourly), 2);
    }

    #[test]
    fn intervals_fall_back_to_elapsed_time_when_first_settlement_is_unknown() {
        let hourly = dec!(1);
        let mut p = position_with(0, 2);
        p.opened_at_wall_ms = NOW_MS - 150 * 60_000; // 2.5 時間前
        p.entry_next_funding_time_ms = None;

        // 経過時間からの近似。最初の 1 回を過大評価しないよう切り捨てる
        assert_eq!(p.intervals_collected(NOW_MS, hourly), 2);
        assert_eq!(p.intervals_collected(p.opened_at_wall_ms, hourly), 0);
        // 精算間隔が不明・非正なら数えられない
        assert_eq!(p.intervals_collected(NOW_MS, Decimal::ZERO), 0);
    }

    #[test]
    fn longer_interval_is_used_when_the_two_dexes_differ() {
        // 両方の精算を跨がないとレート差を完全には取れない。短い方で数えると
        // 回収したつもりで回収できていない。
        let books = BookStore::new();
        books.update(book(Dex::Hyperliquid, dec!(60000)));
        books.update(book(Dex::Lighter, dec!(60000)));

        let funding = FundingStore::new();
        // HL は 1 時間、Lighter は 4 時間精算
        funding.update(rate(Dex::Hyperliquid, dec!(0.00011), dec!(1)));
        funding.update(rate(Dex::Lighter, dec!(0.0001), dec!(4)));

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
        let f = Fixture {
            books,
            funding,
            fees,
            symbols: vec![Symbol::Btc],
            pairs: vec![(Dex::Hyperliquid, Dex::Lighter)],
        };
        let strategy = FundingArbStrategy::new(config());

        // 3 時間保有。1 時間精算なら 3 回だが、4 時間側では 0 回
        assert_eq!(
            strategy.should_exit(&position_with(3, 2), &f.ctx()),
            None,
            "長い方（4 時間）で数えるのでまだ回収できていない"
        );
        // 9 時間なら 4 時間精算でも 2 回跨いでいる
        assert_eq!(
            strategy.should_exit(&position_with(9, 2), &f.ctx()),
            Some(ExitReason::FundingBelowCost)
        );
    }

    #[test]
    fn hysteresis_band_does_not_trigger_an_exit() {
        // レート差が exit(0.3) と entry(1.0) の間にあるとき、回収済みでも
        // 降りない。ここで降りるとエントリー基準を跨ぐたびに建て直しを
        // 繰り返し、往復 6bps の手数料を払い続けることになる。
        let strategy = FundingArbStrategy::new(config());
        // 0.5bps
        let f = Fixture::new(dec!(0.00015), dec!(0.0001), dec!(60000), dec!(60000));
        assert_eq!(strategy.should_exit(&position_with(5, 2), &f.ctx()), None);

        // エントリー側はこの水準では建てない（= 建て直しも起きない）
        assert!(FundingArbStrategy::new(config())
            .evaluate(&f.ctx())
            .is_empty());
    }

    #[test]
    fn exits_below_exit_threshold_after_breakeven() {
        let strategy = FundingArbStrategy::new(config());
        // 0.2bps → exit_rate_diff_bps(0.3) 未満
        let f = Fixture::new(dec!(0.00012), dec!(0.0001), dec!(60000), dec!(60000));
        assert_eq!(
            strategy.should_exit(&position_with(3, 2), &f.ctx()),
            Some(ExitReason::FundingBelowCost)
        );
    }

    /// 決済方向のベーシスが不利な状態（ロング側 Lighter が安く、ショート側
    /// Hyperliquid が高い）。決済すると約 5bps の損を確定させる。
    fn adverse_exit_basis_fixture() -> Fixture {
        // 0.2bps のレート差（exit 閾値 0.3 未満 → FundingBelowCost）
        Fixture::new(dec!(0.00012), dec!(0.0001), dec!(60030), dec!(60000))
    }

    #[test]
    fn defers_low_urgency_exit_while_the_exit_basis_is_adverse() {
        let strategy = FundingArbStrategy::new(config());
        let f = adverse_exit_basis_fixture();

        // 決済方向のベーシスが -5bps（許容 3bps）なので保留する
        let ctx = f.ctx();
        assert!(ctx.signed_exit_basis_bps(&position_with(3, 2)).unwrap() < dec!(-3));
        assert_eq!(
            strategy.should_exit(&position_with(3, 2), &ctx),
            None,
            "有利に戻るまで待つ（Urgency::Patient なので待てる）"
        );

        // ベーシスが許容範囲なら通常どおり降りる
        let ok = Fixture::new(dec!(0.00012), dec!(0.0001), dec!(60000), dec!(60000));
        assert_eq!(
            strategy.should_exit(&position_with(3, 2), &ok.ctx()),
            Some(ExitReason::FundingBelowCost)
        );
    }

    #[test]
    fn deferral_has_an_upper_bound() {
        let strategy = FundingArbStrategy::new(config());
        let f = adverse_exit_basis_fixture();

        // 保留を始めて 5 時間（上限 6 時間）→ まだ待つ
        let mut p = position_with(3, 2);
        p.exit_deferred_since_ms = Some(NOW_MS - 5 * 3_600_000);
        assert_eq!(strategy.should_exit(&p, &f.ctx()), None);

        // 6 時間経過 → ベーシスが不利でも降りる
        p.exit_deferred_since_ms = Some(NOW_MS - 6 * 3_600_000);
        assert_eq!(
            strategy.should_exit(&p, &f.ctx()),
            Some(ExitReason::FundingBelowCost)
        );
    }

    #[test]
    fn urgent_exits_are_never_deferred() {
        let strategy = FundingArbStrategy::new(config());

        // 反転: 決済ベーシスが不利でも即降りる
        let reversed = Fixture::new(dec!(0.0001), dec!(0.0004), dec!(60030), dec!(60000));
        assert_eq!(
            strategy.should_exit(&position_with(3, 2), &reversed.ctx()),
            Some(ExitReason::FundingEdgeGone)
        );

        // データ欠損: API 不調の疑いがあり、待つと状況が悪化しうる
        let f = adverse_exit_basis_fixture();
        let missing = Fixture {
            funding: FundingStore::new(),
            ..f
        };
        assert_eq!(
            strategy.should_exit(&position_with(3, 2), &missing.ctx()),
            Some(ExitReason::FundingDataUnavailable)
        );
    }

    #[test]
    fn stale_books_do_not_defer_the_exit() {
        // エントリーとは安全側の向きが逆。エントリーは「判定できないなら
        // 建てない」が安全だが、エグジットは「判定できないなら待たない」が安全
        // （不確かなデータを根拠に持ち続ける方が危険）。
        let strategy = FundingArbStrategy::new(config());

        let books = BookStore::new();
        books.update(book(Dex::Hyperliquid, dec!(60030)));
        let mut stale = book(Dex::Lighter, dec!(60000));
        stale.trace.received_wall_ms = NOW_MS - 1_500;
        books.update(stale);

        let funding = FundingStore::new();
        funding.update(rate(Dex::Hyperliquid, dec!(0.00012), dec!(1)));
        funding.update(rate(Dex::Lighter, dec!(0.0001), dec!(1)));

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
        let f = Fixture {
            books,
            funding,
            fees,
            symbols: vec![Symbol::Btc],
            pairs: vec![(Dex::Hyperliquid, Dex::Lighter)],
        };

        // ベーシスは不利だが、鮮度差 1500ms > 閾値 1000ms なので保留しない
        assert_eq!(
            strategy.should_exit(&position_with(3, 2), &f.ctx()),
            Some(ExitReason::FundingBelowCost)
        );
    }

    #[test]
    fn max_holding_is_deferrable_but_bounded() {
        let strategy = FundingArbStrategy::new(config());
        let f = adverse_exit_basis_fixture();

        // 保有上限に達していても、決済が不利なら短期間は待てる
        assert_eq!(strategy.should_exit(&position_with(72, 2), &f.ctx()), None);

        // 保留上限を超えたら降りる。実効的な最大保有は
        // max_holding_hours + max_exit_deferral_hours になる
        let mut p = position_with(72, 2);
        p.exit_deferred_since_ms = Some(NOW_MS - 6 * 3_600_000);
        assert_eq!(
            strategy.should_exit(&p, &f.ctx()),
            Some(ExitReason::MaxHoldingReached)
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
    fn exits_when_rate_diff_vanishes() {
        let strategy = FundingArbStrategy::new(config());
        let f = Fixture::new(dec!(0.0004), dec!(0.0001), dec!(60000), dec!(60000));
        // レートが同値になり差が消えた状態を作る（データはある）
        f.funding
            .update(rate(Dex::Hyperliquid, dec!(0.0001), dec!(1)));
        assert_eq!(
            strategy.should_exit(&position_with(1, 2), &f.ctx()),
            Some(ExitReason::FundingEdgeGone),
            "レート差の消滅は回収未達でも降りる"
        );
    }

    #[test]
    fn missing_funding_data_is_distinguished_from_a_vanished_edge() {
        // データ欠損は DEX の API 不調・WS 切断を示唆する。レート差の消滅とは
        // 原因が違うので、リスク管理層が区別できるよう別の理由にする。
        let strategy = FundingArbStrategy::new(config());
        let books = BookStore::new();
        books.update(book(Dex::Hyperliquid, dec!(60000)));
        books.update(book(Dex::Lighter, dec!(60000)));

        // Lighter のレートだけ受信できている状態
        let funding = FundingStore::new();
        funding.update(rate(Dex::Lighter, dec!(0.0001), dec!(1)));

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
        let f = Fixture {
            books,
            funding,
            fees,
            symbols: vec![Symbol::Btc],
            pairs: vec![(Dex::Hyperliquid, Dex::Lighter)],
        };

        assert_eq!(
            strategy.should_exit(&position_with(1, 2), &f.ctx()),
            Some(ExitReason::FundingDataUnavailable)
        );

        // 両方とも取れていない場合も同じ
        let empty = Fixture {
            funding: FundingStore::new(),
            ..f
        };
        assert_eq!(
            strategy.should_exit(&position_with(1, 2), &empty.ctx()),
            Some(ExitReason::FundingDataUnavailable)
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
