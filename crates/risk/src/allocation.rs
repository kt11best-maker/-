//! 戦略ごとの証拠金枠。
//!
//! 2 戦略が同じ証拠金プールを取り合うと、ファンディング裁定の長期ポジションが
//! 価格差アービトラージの機会を潰す（またはその逆）。枠を分離し、各戦略は
//! 自分の枠を超えて発注できないようにする。
//!
//! `reserve_pct` は**どちらの戦略も使えない**。緊急クローズのスリッページを
//! 吸収するための余力として常に確保する。

use config::AllocationConfig;
use rust_decimal::Decimal;
use strategy_traits::StrategyKind;

/// 証拠金枠の配分。
#[derive(Debug, Clone)]
pub struct Allocation {
    price_arb_pct: Decimal,
    funding_arb_pct: Decimal,
    reserve_pct: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllocationError {
    /// 比率が負。
    Negative(&'static str),
    /// 合計が 1.0 を超えている。
    TotalExceedsOne(Decimal),
}

impl std::fmt::Display for AllocationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AllocationError::Negative(name) => write!(f, "{name} は 0 以上にしてください"),
            AllocationError::TotalExceedsOne(total) => {
                write!(f, "配分の合計が 1.0 を超えています: {total}")
            }
        }
    }
}

impl std::error::Error for AllocationError {}

impl Allocation {
    pub fn new(cfg: &AllocationConfig) -> Result<Self, AllocationError> {
        for (name, value) in [
            ("price_arb_pct", cfg.price_arb_pct),
            ("funding_arb_pct", cfg.funding_arb_pct),
            ("reserve_pct", cfg.reserve_pct),
        ] {
            if value < Decimal::ZERO {
                return Err(AllocationError::Negative(name));
            }
        }
        let total = cfg.price_arb_pct + cfg.funding_arb_pct + cfg.reserve_pct;
        if total > Decimal::ONE {
            return Err(AllocationError::TotalExceedsOne(total));
        }
        Ok(Allocation {
            price_arb_pct: cfg.price_arb_pct,
            funding_arb_pct: cfg.funding_arb_pct,
            reserve_pct: cfg.reserve_pct,
        })
    }

    pub fn pct_for(&self, strategy: StrategyKind) -> Decimal {
        match strategy {
            StrategyKind::PriceArb => self.price_arb_pct,
            StrategyKind::FundingArb => self.funding_arb_pct,
        }
    }

    pub fn reserve_pct(&self) -> Decimal {
        self.reserve_pct
    }

    /// 証拠金プールに対する、その戦略が使える上限額。
    pub fn budget_for(&self, strategy: StrategyKind, pool: Decimal) -> Decimal {
        (pool.max(Decimal::ZERO)) * self.pct_for(strategy)
    }

    /// 緊急クローズ用に常に確保しておく額。
    pub fn reserve_for(&self, pool: Decimal) -> Decimal {
        pool.max(Decimal::ZERO) * self.reserve_pct
    }

    /// どちらの戦略にも割り当てられていない余り（reserve を含む）。
    pub fn unallocated_pct(&self) -> Decimal {
        Decimal::ONE - self.price_arb_pct - self.funding_arb_pct - self.reserve_pct
    }
}

/// 戦略ごとの使用額を追跡し、枠を超える発注を止める。
///
/// 証拠金プールの空き容量チェックは全銘柄共通で一元管理する（早い者勝ちで
/// 証拠金を食い尽くさないようにする）。
#[derive(Debug, Clone)]
pub struct MarginBudget {
    allocation: Allocation,
    /// 対象 DEX の証拠金プールのうち少ない方（両建てなので少ない方が効く）。
    pool: Decimal,
    price_arb_used: Decimal,
    funding_arb_used: Decimal,
}

/// 枠のチェック結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetDecision {
    Allowed,
    /// 戦略枠を超える。
    ExceedsStrategyBudget {
        requested: Decimal,
        available: Decimal,
    },
    /// 要求額が 0 以下。
    NonPositive,
}

impl BudgetDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, BudgetDecision::Allowed)
    }
}

impl MarginBudget {
    pub fn new(allocation: Allocation, pool: Decimal) -> Self {
        MarginBudget {
            allocation,
            pool: pool.max(Decimal::ZERO),
            price_arb_used: Decimal::ZERO,
            funding_arb_used: Decimal::ZERO,
        }
    }

    pub fn pool(&self) -> Decimal {
        self.pool
    }

    /// 証拠金プールの残高が変わったら更新する。
    pub fn set_pool(&mut self, pool: Decimal) {
        self.pool = pool.max(Decimal::ZERO);
    }

    fn used(&self, strategy: StrategyKind) -> Decimal {
        match strategy {
            StrategyKind::PriceArb => self.price_arb_used,
            StrategyKind::FundingArb => self.funding_arb_used,
        }
    }

    fn used_mut(&mut self, strategy: StrategyKind) -> &mut Decimal {
        match strategy {
            StrategyKind::PriceArb => &mut self.price_arb_used,
            StrategyKind::FundingArb => &mut self.funding_arb_used,
        }
    }

    /// その戦略がまだ使える額。
    pub fn available(&self, strategy: StrategyKind) -> Decimal {
        (self.allocation.budget_for(strategy, self.pool) - self.used(strategy)).max(Decimal::ZERO)
    }

    /// 発注してよいか判定する（状態は変えない）。
    pub fn check(&self, strategy: StrategyKind, notional: Decimal) -> BudgetDecision {
        if notional <= Decimal::ZERO {
            return BudgetDecision::NonPositive;
        }
        let available = self.available(strategy);
        if notional > available {
            return BudgetDecision::ExceedsStrategyBudget {
                requested: notional,
                available,
            };
        }
        BudgetDecision::Allowed
    }

    /// 枠を確保する。確保できなければ理由を返す。
    pub fn reserve(&mut self, strategy: StrategyKind, notional: Decimal) -> BudgetDecision {
        let decision = self.check(strategy, notional);
        if decision.is_allowed() {
            *self.used_mut(strategy) += notional;
        }
        decision
    }

    /// ポジションを解消したら枠を返す。
    pub fn release(&mut self, strategy: StrategyKind, notional: Decimal) {
        let used = self.used_mut(strategy);
        *used = (*used - notional).max(Decimal::ZERO);
    }

    pub fn used_total(&self) -> Decimal {
        self.price_arb_used + self.funding_arb_used
    }

    /// 緊急クローズ用の余力が侵食されていないか。
    pub fn reserve_intact(&self) -> bool {
        self.pool - self.used_total() >= self.allocation.reserve_for(self.pool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn cfg(price: Decimal, funding: Decimal, reserve: Decimal) -> AllocationConfig {
        AllocationConfig {
            price_arb_pct: price,
            funding_arb_pct: funding,
            reserve_pct: reserve,
        }
    }

    fn allocation() -> Allocation {
        Allocation::new(&cfg(dec!(0.30), dec!(0.40), dec!(0.30))).unwrap()
    }

    #[test]
    fn rejects_over_allocation() {
        let err = Allocation::new(&cfg(dec!(0.5), dec!(0.4), dec!(0.3))).unwrap_err();
        assert_eq!(err, AllocationError::TotalExceedsOne(dec!(1.2)));
    }

    #[test]
    fn rejects_negative_share() {
        assert_eq!(
            Allocation::new(&cfg(dec!(-0.1), dec!(0.4), dec!(0.3))).unwrap_err(),
            AllocationError::Negative("price_arb_pct")
        );
    }

    #[test]
    fn allows_leaving_headroom() {
        let a = Allocation::new(&cfg(dec!(0.2), dec!(0.2), dec!(0.2))).unwrap();
        assert_eq!(a.unallocated_pct(), dec!(0.4));
    }

    #[test]
    fn budgets_are_separated_per_strategy() {
        let a = allocation();
        assert_eq!(
            a.budget_for(StrategyKind::PriceArb, dec!(10_000)),
            dec!(3_000)
        );
        assert_eq!(
            a.budget_for(StrategyKind::FundingArb, dec!(10_000)),
            dec!(4_000)
        );
        assert_eq!(a.reserve_for(dec!(10_000)), dec!(3_000));
    }

    #[test]
    fn reserve_and_release_track_usage() {
        let mut budget = MarginBudget::new(allocation(), dec!(10_000));
        assert_eq!(budget.available(StrategyKind::PriceArb), dec!(3_000));

        assert_eq!(
            budget.reserve(StrategyKind::PriceArb, dec!(1_000)),
            BudgetDecision::Allowed
        );
        assert_eq!(budget.available(StrategyKind::PriceArb), dec!(2_000));
        // 他戦略の枠は減らない
        assert_eq!(budget.available(StrategyKind::FundingArb), dec!(4_000));

        budget.release(StrategyKind::PriceArb, dec!(1_000));
        assert_eq!(budget.available(StrategyKind::PriceArb), dec!(3_000));
    }

    #[test]
    fn one_strategy_cannot_eat_the_others_budget() {
        let mut budget = MarginBudget::new(allocation(), dec!(10_000));
        // ファンディング裁定が枠いっぱいまで使っても…
        assert!(budget
            .reserve(StrategyKind::FundingArb, dec!(4_000))
            .is_allowed());
        assert_eq!(budget.available(StrategyKind::FundingArb), Decimal::ZERO);
        // 価格差アービトラージの枠は無傷
        assert!(budget
            .reserve(StrategyKind::PriceArb, dec!(3_000))
            .is_allowed());

        // 枠を超える要求は理由付きで拒否される
        assert_eq!(
            budget.reserve(StrategyKind::FundingArb, dec!(1)),
            BudgetDecision::ExceedsStrategyBudget {
                requested: dec!(1),
                available: Decimal::ZERO
            }
        );
    }

    #[test]
    fn reserve_pct_is_never_usable() {
        let mut budget = MarginBudget::new(allocation(), dec!(10_000));
        budget.reserve(StrategyKind::PriceArb, dec!(3_000));
        budget.reserve(StrategyKind::FundingArb, dec!(4_000));
        // 7,000 使っても 3,000 の緊急クローズ余力が残る
        assert_eq!(budget.used_total(), dec!(7_000));
        assert!(budget.reserve_intact());
    }

    #[test]
    fn shrinking_pool_shrinks_budgets() {
        let mut budget = MarginBudget::new(allocation(), dec!(10_000));
        budget.reserve(StrategyKind::PriceArb, dec!(3_000));
        // 含み損などでプールが半減すると、枠も半分（1,500）になり、
        // 既に 3,000 使っているので新規は出せない
        budget.set_pool(dec!(5_000));
        assert_eq!(budget.available(StrategyKind::PriceArb), Decimal::ZERO);
        // ただし緊急クローズ余力（1,500）はまだ残っている: 5,000 - 3,000 = 2,000
        assert!(budget.reserve_intact());

        // さらに減ると余力そのものが侵食される: 3,500 - 3,000 = 500 < 1,050
        budget.set_pool(dec!(3_500));
        assert!(!budget.reserve_intact(), "余力が侵食されたことを検知する");
    }

    #[test]
    fn non_positive_requests_are_rejected() {
        let budget = MarginBudget::new(allocation(), dec!(10_000));
        assert_eq!(
            budget.check(StrategyKind::PriceArb, Decimal::ZERO),
            BudgetDecision::NonPositive
        );
        assert_eq!(
            budget.check(StrategyKind::PriceArb, dec!(-1)),
            BudgetDecision::NonPositive
        );
    }
}
