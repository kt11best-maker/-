//! 多層キルスイッチ。
//!
//! **戦略ごとに独立して発動できる**のがこの層の要点。価格差アービトラージが
//! 不調でも、ファンディング裁定は継続してよい場合がある。一方で Bot 全体の
//! 日次損失上限に達したら両戦略を止める。
//!
//! 発動条件・閾値は設定ファイルで外出しにしてある。実際の発動時に通知を送るのは
//! 上位（`notifier`）の仕事で、ここは判定と状態保持だけを行う。

use config::KillSwitchConfig;
use rust_decimal::Decimal;
use strategy_traits::StrategyKind;

/// 停止の理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HaltReason {
    /// その戦略の日次損失上限に到達。
    StrategyDailyLoss,
    /// Bot 全体の日次損失上限に到達（両戦略を停止）。
    GlobalDailyLoss,
    /// 証拠金維持率の悪化。
    MarginPressure,
    /// 手動停止。
    Manual,
}

impl HaltReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            HaltReason::StrategyDailyLoss => "strategy_daily_loss",
            HaltReason::GlobalDailyLoss => "global_daily_loss",
            HaltReason::MarginPressure => "margin_pressure",
            HaltReason::Manual => "manual",
        }
    }
}

/// 発注してよいか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradingState {
    Running,
    Halted(HaltReason),
}

impl TradingState {
    pub fn is_running(&self) -> bool {
        matches!(self, TradingState::Running)
    }

    pub fn halt_reason(&self) -> Option<HaltReason> {
        match self {
            TradingState::Halted(reason) => Some(*reason),
            TradingState::Running => None,
        }
    }
}

/// 戦略別 + 全体のキルスイッチ。
///
/// 損益は「証拠金プールに対する %」で扱う。損失は負の値で渡すこと。
#[derive(Debug, Clone)]
pub struct KillSwitch {
    cfg: KillSwitchConfig,
    price_arb: TradingState,
    funding_arb: TradingState,
    global: TradingState,
}

impl KillSwitch {
    pub fn new(cfg: KillSwitchConfig) -> Self {
        KillSwitch {
            cfg,
            price_arb: TradingState::Running,
            funding_arb: TradingState::Running,
            global: TradingState::Running,
        }
    }

    /// その戦略が新規発注してよいか。全体停止は個別状態より優先する。
    pub fn state(&self, strategy: StrategyKind) -> TradingState {
        if let TradingState::Halted(reason) = self.global {
            return TradingState::Halted(reason);
        }
        match strategy {
            StrategyKind::PriceArb => self.price_arb,
            StrategyKind::FundingArb => self.funding_arb,
        }
    }

    pub fn can_trade(&self, strategy: StrategyKind) -> bool {
        self.state(strategy).is_running()
    }

    pub fn global_state(&self) -> TradingState {
        self.global
    }

    fn strategy_limit(&self, strategy: StrategyKind) -> Decimal {
        match strategy {
            StrategyKind::PriceArb => self.cfg.price_arb_daily_loss_limit_pct,
            StrategyKind::FundingArb => self.cfg.funding_arb_daily_loss_limit_pct,
        }
    }

    fn state_mut(&mut self, strategy: StrategyKind) -> &mut TradingState {
        match strategy {
            StrategyKind::PriceArb => &mut self.price_arb,
            StrategyKind::FundingArb => &mut self.funding_arb,
        }
    }

    /// 日次損益（%）を反映する。損失は負で渡す。
    ///
    /// 戻り値は、このタイミングで**新たに**停止した戦略。通知を出す判断に使う。
    pub fn record_daily_pnl_pct(
        &mut self,
        price_arb_pct: Decimal,
        funding_arb_pct: Decimal,
    ) -> Vec<(StrategyKind, HaltReason)> {
        let mut newly_halted = Vec::new();

        // Bot 全体（両戦略を停止）
        let total = price_arb_pct + funding_arb_pct;
        if self.global.is_running() && total <= -self.cfg.global_daily_loss_limit_pct {
            self.global = TradingState::Halted(HaltReason::GlobalDailyLoss);
            for strategy in StrategyKind::ALL {
                newly_halted.push((strategy, HaltReason::GlobalDailyLoss));
            }
            return newly_halted;
        }

        // 戦略ごと
        for (strategy, pnl) in [
            (StrategyKind::PriceArb, price_arb_pct),
            (StrategyKind::FundingArb, funding_arb_pct),
        ] {
            let limit = self.strategy_limit(strategy);
            let state = self.state_mut(strategy);
            if state.is_running() && pnl <= -limit {
                *state = TradingState::Halted(HaltReason::StrategyDailyLoss);
                newly_halted.push((strategy, HaltReason::StrategyDailyLoss));
            }
        }
        newly_halted
    }

    /// 証拠金維持率の悪化などで、戦略横断に止める。
    ///
    /// ファンディング裁定の長期ポジションが維持率を圧迫している状況で価格差
    /// アービトラージが発注するのは危険なので、これは全体停止として扱う。
    pub fn halt_all(&mut self, reason: HaltReason) {
        if self.global.is_running() {
            self.global = TradingState::Halted(reason);
        }
    }

    /// 片方の戦略だけを止める。
    pub fn halt_strategy(&mut self, strategy: StrategyKind, reason: HaltReason) {
        let state = self.state_mut(strategy);
        if state.is_running() {
            *state = TradingState::Halted(reason);
        }
    }

    /// 日次リセット（運用者の判断で再開する場合も含む）。
    pub fn reset(&mut self) {
        self.price_arb = TradingState::Running;
        self.funding_arb = TradingState::Running;
        self.global = TradingState::Running;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn switch() -> KillSwitch {
        KillSwitch::new(KillSwitchConfig {
            price_arb_daily_loss_limit_pct: dec!(2),
            funding_arb_daily_loss_limit_pct: dec!(2),
            global_daily_loss_limit_pct: dec!(3),
        })
    }

    #[test]
    fn starts_running() {
        let k = switch();
        assert!(k.can_trade(StrategyKind::PriceArb));
        assert!(k.can_trade(StrategyKind::FundingArb));
    }

    #[test]
    fn halts_only_the_losing_strategy() {
        let mut k = switch();
        // 価格差だけが 2% 負け、ファンディングは +0.5%
        let halted = k.record_daily_pnl_pct(dec!(-2), dec!(0.5));
        assert_eq!(
            halted,
            vec![(StrategyKind::PriceArb, HaltReason::StrategyDailyLoss)]
        );

        assert!(!k.can_trade(StrategyKind::PriceArb));
        assert!(
            k.can_trade(StrategyKind::FundingArb),
            "もう片方は継続してよい"
        );
    }

    #[test]
    fn global_limit_halts_both() {
        let mut k = switch();
        // 単体では上限未満だが、合計が -3% に達する
        let halted = k.record_daily_pnl_pct(dec!(-1.5), dec!(-1.5));
        assert_eq!(halted.len(), 2);
        assert!(!k.can_trade(StrategyKind::PriceArb));
        assert!(!k.can_trade(StrategyKind::FundingArb));
        assert_eq!(
            k.global_state().halt_reason(),
            Some(HaltReason::GlobalDailyLoss)
        );
    }

    #[test]
    fn halting_is_reported_only_once() {
        let mut k = switch();
        assert_eq!(k.record_daily_pnl_pct(dec!(-2), dec!(0)).len(), 1);
        // すでに停止済みなら再通知しない
        assert!(k.record_daily_pnl_pct(dec!(-2.5), dec!(0)).is_empty());
    }

    #[test]
    fn margin_pressure_halts_everything() {
        let mut k = switch();
        k.halt_all(HaltReason::MarginPressure);
        assert!(!k.can_trade(StrategyKind::PriceArb));
        assert!(!k.can_trade(StrategyKind::FundingArb));
        assert_eq!(
            k.state(StrategyKind::FundingArb).halt_reason(),
            Some(HaltReason::MarginPressure)
        );
    }

    #[test]
    fn global_halt_overrides_individual_state() {
        let mut k = switch();
        k.halt_strategy(StrategyKind::PriceArb, HaltReason::Manual);
        assert!(k.can_trade(StrategyKind::FundingArb));

        k.halt_all(HaltReason::GlobalDailyLoss);
        // 全体停止が個別状態より優先される
        assert_eq!(
            k.state(StrategyKind::PriceArb).halt_reason(),
            Some(HaltReason::GlobalDailyLoss)
        );
    }

    #[test]
    fn profit_does_not_halt() {
        let mut k = switch();
        assert!(k.record_daily_pnl_pct(dec!(1.5), dec!(2.0)).is_empty());
        assert!(k.can_trade(StrategyKind::PriceArb));
    }

    #[test]
    fn reset_resumes_trading() {
        let mut k = switch();
        k.record_daily_pnl_pct(dec!(-5), dec!(-5));
        k.reset();
        assert!(k.can_trade(StrategyKind::PriceArb));
        assert!(k.can_trade(StrategyKind::FundingArb));
    }
}
