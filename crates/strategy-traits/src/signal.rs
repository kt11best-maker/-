use std::time::Instant;

use core_types::{Dex, Symbol};
use rust_decimal::Decimal;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StrategyKind {
    PriceArb,
    FundingArb,
}

impl StrategyKind {
    pub const ALL: [StrategyKind; 2] = [StrategyKind::PriceArb, StrategyKind::FundingArb];

    pub fn as_str(&self) -> &'static str {
        match self {
            StrategyKind::PriceArb => "price_arb",
            StrategyKind::FundingArb => "funding_arb",
        }
    }
}

impl std::fmt::Display for StrategyKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 執行の緊急度。執行方式の選択に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    /// 価格差アービトラージ用。IOC で即時執行し、間に合わなければ諦める。
    Immediate,
    /// ファンディング裁定用。指値でじっくり建てる。数分待ってよい。
    Patient,
}

impl Urgency {
    pub fn as_str(&self) -> &'static str {
        match self {
            Urgency::Immediate => "immediate",
            Urgency::Patient => "patient",
        }
    }
}

/// 戦略が発するシグナル。
///
/// # `expected_profit_bps` の意味の違い
///
/// - 価格差アービトラージ: **1 回の往復で得られる**純利益（bps）
/// - ファンディング裁定: **1 精算あたりの**純収益（bps）。年率ではない
///
/// **この 2 つを同じ土俵で比較してはいけない。** 資金配分を判断する際は、
/// ファンディング側を「想定保有精算回数 × 1 回あたり収益」に換算すること
/// （[`TradeSignal::total_expected_profit_bps`] を使う）。
#[derive(Debug, Clone)]
pub struct TradeSignal {
    pub strategy: StrategyKind,
    pub symbol: Symbol,
    /// ロングを建てる側。
    pub long_dex: Dex,
    /// ショートを建てる側。
    pub short_dex: Dex,
    /// 想定ノーショナル（USD 建て）。
    pub notional: Decimal,
    /// 期待収益（bps）。戦略により意味が異なる（上記参照）。
    pub expected_profit_bps: Decimal,
    /// 執行の緊急度。
    pub urgency: Urgency,
    /// シグナル生成時点の根拠（ログ・分析用）。
    pub rationale: SignalRationale,
    pub created_at: Instant,
    pub created_at_wall_ms: u64,
}

impl TradeSignal {
    /// 資金配分の比較に使う、**保有期間全体**の期待収益（bps）。
    ///
    /// 価格差はそのまま（1 往復で完結）。ファンディングは
    /// 「1 精算あたり × 想定精算回数」に換算する。
    pub fn total_expected_profit_bps(&self) -> Decimal {
        match &self.rationale {
            SignalRationale::PriceArb { .. } => self.expected_profit_bps,
            SignalRationale::FundingArb {
                expected_intervals, ..
            } => self.expected_profit_bps * Decimal::from(*expected_intervals),
        }
    }
}

/// シグナルの根拠。CSV / ログに落として後から検証できるようにする。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalRationale {
    PriceArb {
        /// 手数料等を引く前の乖離（bps）。VWAP ベースならスリッページ込み。
        gross_spread_bps: Decimal,
        /// 差し引いた手数料（bps）。
        fee_bps: Decimal,
        /// 差し引いたスリッページ（bps）。VWAP ベースの場合は 0
        /// （gross に織り込み済み）。
        slippage_bps: Decimal,
        buffer_bps: Decimal,
        /// 両 DEX の板の鮮度差。大きいほど「見かけ上の乖離」の疑いが強い。
        staleness_delta_ms: i64,
        /// 乖離が VWAP ベースか best 気配ベースか。
        vwap_based: bool,
    },
    FundingArb {
        /// 1 精算あたりのレート差（bps）。
        rate_diff_bps: Decimal,
        /// 想定保有精算回数。
        expected_intervals: u32,
        /// 手数料回収に必要な最低精算回数。
        breakeven_intervals: u32,
        /// 建てる方向で見た価格差（bps）。正なら有利、負なら不利。
        basis_entry_bps: Decimal,
        /// 期待収益に加算した有利ベーシス（保守評価では常に 0）。
        basis_credit_bps: Decimal,
        /// 建て + 決済の手数料合計（bps, 4 レグ）。
        fee_bps: Decimal,
        buffer_bps: Decimal,
        /// 両 DEX の精算間隔が同じか。
        same_interval: bool,
    },
}

/// 保有中のポジション（`should_exit` の判定対象）。
///
/// 執行レイヤー（フェーズ3）が実際の建玉から組み立てる。フェーズ2 の
/// ドライランでは仮想ポジションとして使う。
#[derive(Debug, Clone)]
pub struct OpenPosition {
    pub strategy: StrategyKind,
    pub symbol: Symbol,
    pub long_dex: Dex,
    pub short_dex: Dex,
    pub notional: Decimal,
    /// 建てた時点の価格差（bps, 符号付き）。ベーシス項の評価に使う。
    pub entry_basis_bps: Decimal,
    /// 建てた時点で見込んだ 1 精算あたりのレート差（bps）。
    pub entry_rate_diff_bps: Decimal,
    pub opened_at_wall_ms: u64,
    /// これまでに跨いだ精算回数。
    pub funding_intervals_collected: u32,
}

impl OpenPosition {
    /// 保有時間（時間）。
    pub fn holding_hours(&self, now_wall_ms: u64) -> Decimal {
        let elapsed_ms = now_wall_ms.saturating_sub(self.opened_at_wall_ms);
        Decimal::from(elapsed_ms) / Decimal::from(3_600_000u32)
    }
}

/// ポジションを解消する理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// レート差が消滅・反転した（保有理由がなくなった）。
    FundingEdgeGone,
    /// レート差が決済コストを下回った。
    FundingBelowCost,
    /// 価格差が目標まで収束した（価格差アービトラージの利確）。
    SpreadConverged,
    /// 最大保有期間に到達。
    MaxHoldingReached,
    /// 証拠金維持率の悪化（リスク管理層からの指示）。
    MarginPressure,
    /// キルスイッチ発動。
    KillSwitch,
}

impl ExitReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExitReason::FundingEdgeGone => "funding_edge_gone",
            ExitReason::FundingBelowCost => "funding_below_cost",
            ExitReason::SpreadConverged => "spread_converged",
            ExitReason::MaxHoldingReached => "max_holding_reached",
            ExitReason::MarginPressure => "margin_pressure",
            ExitReason::KillSwitch => "kill_switch",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn signal(strategy: StrategyKind, rationale: SignalRationale, profit: Decimal) -> TradeSignal {
        TradeSignal {
            strategy,
            symbol: Symbol::Btc,
            long_dex: Dex::Lighter,
            short_dex: Dex::Hyperliquid,
            notional: dec!(1000),
            expected_profit_bps: profit,
            urgency: Urgency::Immediate,
            rationale,
            created_at: Instant::now(),
            created_at_wall_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn price_arb_profit_is_per_round_trip() {
        let s = signal(
            StrategyKind::PriceArb,
            SignalRationale::PriceArb {
                gross_spread_bps: dec!(20),
                fee_bps: dec!(13),
                slippage_bps: dec!(0),
                buffer_bps: dec!(2),
                staleness_delta_ms: 5,
                vwap_based: true,
            },
            dec!(5),
        );
        // 1 往復で完結するので換算しない
        assert_eq!(s.total_expected_profit_bps(), dec!(5));
    }

    #[test]
    fn funding_arb_profit_is_scaled_by_intervals() {
        let s = signal(
            StrategyKind::FundingArb,
            SignalRationale::FundingArb {
                rate_diff_bps: dec!(2),
                expected_intervals: 12,
                breakeven_intervals: 5,
                basis_entry_bps: dec!(1),
                basis_credit_bps: dec!(0),
                fee_bps: dec!(9),
                buffer_bps: dec!(1),
                same_interval: true,
            },
            dec!(1.5),
        );
        // 1 精算 1.5bps × 12 回 = 18bps。価格差の 5bps と比較できるのはこちら
        assert_eq!(s.total_expected_profit_bps(), dec!(18));
        assert!(s.total_expected_profit_bps() > s.expected_profit_bps);
    }

    #[test]
    fn holding_hours() {
        let p = OpenPosition {
            strategy: StrategyKind::FundingArb,
            symbol: Symbol::Btc,
            long_dex: Dex::Lighter,
            short_dex: Dex::Hyperliquid,
            notional: dec!(1000),
            entry_basis_bps: dec!(1),
            entry_rate_diff_bps: dec!(2),
            opened_at_wall_ms: 1_700_000_000_000,
            funding_intervals_collected: 3,
        };
        // 3 時間半後
        assert_eq!(
            p.holding_hours(1_700_000_000_000 + 3 * 3_600_000 + 1_800_000),
            dec!(3.5)
        );
        // 時計が巻き戻っても負にならない
        assert_eq!(p.holding_hours(1_600_000_000_000), Decimal::ZERO);
    }
}
