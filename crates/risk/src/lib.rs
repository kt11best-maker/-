//! リスク管理。
//!
//! 2 戦略が同じ証拠金プールを共有するため、この層で
//!
//! - **戦略ごとの証拠金枠**（[`allocation`]）
//! - **戦略ごとに独立して発動できるキルスイッチ**（[`killswitch`]）
//!
//! を持つ。証拠金維持率の監視は戦略横断で行い、悪化時は全体を止める。
//!
//! > **フェーズ1 時点の位置づけ**: 執行レイヤーがまだ無いため、ここは純粋な
//! > 判定ロジックとして実装してある。フェーズ3 で `execution` を作る際に、
//! > 発注前チェックとしてこの crate を呼ぶ。

pub mod allocation;
pub mod killswitch;

pub use allocation::{Allocation, AllocationError, BudgetDecision, MarginBudget};
pub use killswitch::{HaltReason, KillSwitch, TradingState};

use rust_decimal::Decimal;
use strategy_traits::{StrategyKind, TradeSignal};

/// 発注前チェックの結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RiskDecision {
    Approved {
        /// 枠に収まるよう縮小されたノーショナル。
        notional: Decimal,
    },
    /// キルスイッチで停止中。
    Halted(HaltReason),
    /// 戦略枠に空きが無い。
    NoBudget {
        requested: Decimal,
        available: Decimal,
    },
}

impl RiskDecision {
    pub fn is_approved(&self) -> bool {
        matches!(self, RiskDecision::Approved { .. })
    }

    pub fn notional(&self) -> Option<Decimal> {
        match self {
            RiskDecision::Approved { notional } => Some(*notional),
            _ => None,
        }
    }
}

/// キルスイッチと証拠金枠をまとめた発注前ゲート。
#[derive(Debug, Clone)]
pub struct RiskGate {
    kill_switch: KillSwitch,
    budget: MarginBudget,
}

impl RiskGate {
    pub fn new(kill_switch: KillSwitch, budget: MarginBudget) -> Self {
        RiskGate {
            kill_switch,
            budget,
        }
    }

    pub fn kill_switch(&self) -> &KillSwitch {
        &self.kill_switch
    }

    pub fn kill_switch_mut(&mut self) -> &mut KillSwitch {
        &mut self.kill_switch
    }

    pub fn budget(&self) -> &MarginBudget {
        &self.budget
    }

    pub fn budget_mut(&mut self) -> &mut MarginBudget {
        &mut self.budget
    }

    /// シグナルを発注してよいか判定する。
    ///
    /// 枠に一部しか収まらない場合は**縮小して承認**する（機会を捨てずに済む）。
    /// 枠がまったく無い場合のみ拒否。
    pub fn evaluate(&self, signal: &TradeSignal) -> RiskDecision {
        if let Some(reason) = self.kill_switch.state(signal.strategy).halt_reason() {
            return RiskDecision::Halted(reason);
        }
        let available = self.budget.available(signal.strategy);
        if available <= Decimal::ZERO {
            return RiskDecision::NoBudget {
                requested: signal.notional,
                available,
            };
        }
        RiskDecision::Approved {
            notional: signal.notional.min(available),
        }
    }

    /// 承認された額で枠を確保する。
    pub fn commit(&mut self, strategy: StrategyKind, notional: Decimal) -> BudgetDecision {
        self.budget.reserve(strategy, notional)
    }

    /// ポジション解消時に枠を返す。
    pub fn release(&mut self, strategy: StrategyKind, notional: Decimal) {
        self.budget.release(strategy, notional);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::{AllocationConfig, KillSwitchConfig};
    use core_types::{Dex, Symbol};
    use rust_decimal_macros::dec;
    use std::time::Instant;
    use strategy_traits::{SignalRationale, Urgency};

    fn signal(strategy: StrategyKind, notional: Decimal) -> TradeSignal {
        TradeSignal {
            strategy,
            symbol: Symbol::Btc,
            long_dex: Dex::Lighter,
            short_dex: Dex::Hyperliquid,
            notional,
            expected_profit_bps: dec!(3),
            urgency: Urgency::Immediate,
            rationale: SignalRationale::PriceArb {
                gross_spread_bps: dec!(10),
                fee_bps: dec!(4),
                slippage_bps: Decimal::ZERO,
                buffer_bps: dec!(1),
                staleness_delta_ms: 0,
                vwap_based: true,
            },
            created_at: Instant::now(),
            created_at_wall_ms: 1_700_000_000_000,
        }
    }

    fn gate(pool: Decimal) -> RiskGate {
        let allocation = Allocation::new(&AllocationConfig {
            price_arb_pct: dec!(0.30),
            funding_arb_pct: dec!(0.40),
            reserve_pct: dec!(0.30),
        })
        .unwrap();
        let kill_switch = KillSwitch::new(KillSwitchConfig {
            price_arb_daily_loss_limit_pct: dec!(2),
            funding_arb_daily_loss_limit_pct: dec!(2),
            global_daily_loss_limit_pct: dec!(3),
        });
        RiskGate::new(kill_switch, MarginBudget::new(allocation, pool))
    }

    #[test]
    fn approves_within_budget() {
        let gate = gate(dec!(10_000));
        let decision = gate.evaluate(&signal(StrategyKind::PriceArb, dec!(1_000)));
        assert_eq!(
            decision,
            RiskDecision::Approved {
                notional: dec!(1_000)
            }
        );
    }

    #[test]
    fn shrinks_to_available_budget() {
        let mut gate = gate(dec!(10_000));
        // 枠 3,000 のうち 2,500 を使用済み
        gate.commit(StrategyKind::PriceArb, dec!(2_500));
        let decision = gate.evaluate(&signal(StrategyKind::PriceArb, dec!(1_000)));
        // 機会を捨てずに 500 まで縮小して承認する
        assert_eq!(decision.notional(), Some(dec!(500)));
    }

    #[test]
    fn rejects_when_budget_is_exhausted() {
        let mut gate = gate(dec!(10_000));
        gate.commit(StrategyKind::PriceArb, dec!(3_000));
        let decision = gate.evaluate(&signal(StrategyKind::PriceArb, dec!(1_000)));
        assert_eq!(
            decision,
            RiskDecision::NoBudget {
                requested: dec!(1_000),
                available: Decimal::ZERO
            }
        );
        // 別戦略はまだ発注できる
        assert!(gate
            .evaluate(&signal(StrategyKind::FundingArb, dec!(1_000)))
            .is_approved());
    }

    #[test]
    fn halted_strategy_is_blocked_but_the_other_continues() {
        let mut gate = gate(dec!(10_000));
        gate.kill_switch_mut()
            .record_daily_pnl_pct(dec!(-2), dec!(0));

        assert_eq!(
            gate.evaluate(&signal(StrategyKind::PriceArb, dec!(100))),
            RiskDecision::Halted(HaltReason::StrategyDailyLoss)
        );
        assert!(gate
            .evaluate(&signal(StrategyKind::FundingArb, dec!(100)))
            .is_approved());
    }

    #[test]
    fn global_halt_blocks_everything() {
        let mut gate = gate(dec!(10_000));
        gate.kill_switch_mut().halt_all(HaltReason::MarginPressure);
        for strategy in StrategyKind::ALL {
            assert_eq!(
                gate.evaluate(&signal(strategy, dec!(100))),
                RiskDecision::Halted(HaltReason::MarginPressure)
            );
        }
    }

    #[test]
    fn release_frees_budget_for_the_next_signal() {
        let mut gate = gate(dec!(10_000));
        gate.commit(StrategyKind::PriceArb, dec!(3_000));
        assert!(!gate
            .evaluate(&signal(StrategyKind::PriceArb, dec!(100)))
            .is_approved());

        gate.release(StrategyKind::PriceArb, dec!(3_000));
        assert!(gate
            .evaluate(&signal(StrategyKind::PriceArb, dec!(100)))
            .is_approved());
    }
}
