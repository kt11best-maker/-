//! ファンディングレートの共通型。
//!
//! # 精算間隔の正規化（最重要）
//!
//! DEX によってファンディングの精算間隔が異なる（1 時間ごと、8 時間ごとなど）。
//! **間隔が違うレートをそのまま比較してはいけない。** 必ず `interval_hours` を
//! 記録し、比較・分析時は年率換算（[`FundingRate::annualized_pct`]）で正規化する。
//! ここを取り違えると「8 倍の差がある」と誤認する。
//!
//! API が間隔を返さない DEX では、設定ファイルの `[funding.intervals_hours]` を
//! フォールバック定数として使う。
//!
//! # 符号の規約
//!
//! `current_rate > 0` = **ロングがショートに支払う**。DEX ごとに規約が違う場合は、
//! 各 dex crate の取り込み時点でこの規約に正規化すること。

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::num::{Price, BPS_DENOMINATOR};
use crate::symbol::{Dex, Symbol};
use crate::trace::MessageTrace;

/// 1 日の時間数。
const HOURS_PER_DAY: Decimal = Decimal::from_parts(24, 0, 0, false, 0);
/// 1 年の日数。
const DAYS_PER_YEAR: Decimal = Decimal::from_parts(365, 0, 0, false, 0);

/// 正規化されたファンディングレート情報。
#[derive(Debug, Clone, Copy)]
pub struct FundingRate {
    pub dex: Dex,
    pub symbol: Symbol,
    /// 現在のファンディングレート（1 回の精算あたりの率。年率換算ではない）。
    /// 符号: 正 = ロングがショートに支払う。
    pub current_rate: Decimal,
    /// 予測レート（API が提供する場合）。
    pub predicted_rate: Option<Decimal>,
    /// 精算間隔（時間）。DEX によって 1h / 8h など異なる。
    pub interval_hours: Option<Decimal>,
    /// 次回精算時刻（wall clock, ms epoch）。API が提供する場合。
    pub next_funding_time_ms: Option<u64>,
    /// インデックス価格・マーク価格（取得できる場合）。
    pub index_price: Option<Price>,
    pub mark_price: Option<Price>,
    pub trace: MessageTrace,
}

impl FundingRate {
    /// 1 精算あたりのレート（bps）。
    pub fn rate_bps(&self) -> Decimal {
        self.current_rate * BPS_DENOMINATOR
    }

    /// 年間の精算回数。`interval_hours` が不明・非正なら `None`。
    pub fn intervals_per_year(&self) -> Option<Decimal> {
        let interval = self.interval_hours?;
        if interval <= Decimal::ZERO {
            return None;
        }
        Some(HOURS_PER_DAY / interval * DAYS_PER_YEAR)
    }

    /// 年率換算したレート（%）。`interval_hours` が不明な場合は `None`。
    ///
    /// `= current_rate * (24 / interval_hours) * 365 * 100`
    pub fn annualized_pct(&self) -> Option<Decimal> {
        Some(self.current_rate * self.intervals_per_year()? * Decimal::ONE_HUNDRED)
    }

    /// 年率換算したレート（bps）。DEX 間比較はこちらで行う。
    pub fn annualized_bps(&self) -> Option<Decimal> {
        Some(self.rate_bps() * self.intervals_per_year()?)
    }

    /// 精算間隔が同じか。**どちらかが不明なら false**
    /// （不明なものを「同じ」と扱うと 1 精算あたりの比較が壊れるため）。
    pub fn same_interval_as(&self, other: &FundingRate) -> bool {
        match (self.interval_hours, other.interval_hours) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }

    /// 取引所 → 受信の遅延（ms）。
    pub fn latency_ms(&self) -> Option<i64> {
        self.trace.exchange_to_local_ms()
    }
}

/// 2 DEX 間のファンディングレート差。
///
/// 「レートが高い DEX でショート、低い DEX でロング」が裁定の建て方。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FundingSpread {
    pub symbol: Symbol,
    /// ロングを建てる側（レートが低い）。
    pub long_dex: Dex,
    /// ショートを建てる側（レートが高い）。
    pub short_dex: Dex,
    /// 1 精算あたりのレート差（bps, 非負）。
    ///
    /// **`same_interval` が false のときは意味を持たない。**
    /// その場合は `annualized_diff_bps` を使うこと。
    pub rate_diff_bps: Decimal,
    /// 年率換算のレート差（bps）。間隔が違う DEX 同士でも比較できる。
    pub annualized_diff_bps: Option<Decimal>,
    /// 両 DEX の精算間隔が同じか（どちらかが不明なら false）。
    pub same_interval: bool,
    /// 精算間隔（時間）。同じ場合のみ `Some`。
    pub interval_hours: Option<Decimal>,
}

impl FundingSpread {
    /// 2 つのレートから裁定の向きと差を求める。
    ///
    /// 銘柄が違う / 同じ DEX 同士 / レート差が 0 の場合は `None`。
    pub fn between(a: &FundingRate, b: &FundingRate) -> Option<FundingSpread> {
        if a.symbol != b.symbol || a.dex == b.dex || a.current_rate == b.current_rate {
            return None;
        }

        // レートが高い方でショート（受け取る側）、低い方でロング
        let (short, long) = if a.current_rate > b.current_rate {
            (a, b)
        } else {
            (b, a)
        };

        let same_interval = a.same_interval_as(b);
        let annualized_diff_bps = match (short.annualized_bps(), long.annualized_bps()) {
            (Some(s), Some(l)) => Some(s - l),
            _ => None,
        };

        Some(FundingSpread {
            symbol: a.symbol,
            long_dex: long.dex,
            short_dex: short.dex,
            rate_diff_bps: short.rate_bps() - long.rate_bps(),
            annualized_diff_bps,
            same_interval,
            interval_hours: same_interval.then_some(a.interval_hours).flatten(),
        })
    }

    /// 手数料を回収するのに必要な最低精算回数。
    pub fn breakeven_intervals(&self, total_fee_bps: Decimal) -> Option<u32> {
        breakeven_intervals(self.rate_diff_bps, total_fee_bps)
    }
}

/// 手数料を回収するのに必要な最低精算回数。
///
/// 1 精算あたりのレート差が手数料合計を下回る場合、**最低でも何回精算を
/// またぐ必要があるか**を返す。呼び出し側は、それだけ保有し続けられる前提で
/// なければシグナルを出してはいけない。
pub fn breakeven_intervals(rate_diff_bps: Decimal, total_fee_bps: Decimal) -> Option<u32> {
    if rate_diff_bps <= Decimal::ZERO {
        return None;
    }
    if total_fee_bps <= Decimal::ZERO {
        return Some(0);
    }
    let intervals = (total_fee_bps / rate_diff_bps).ceil();
    u32::try_from(intervals.trunc().mantissa()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn rate(dex: Dex, current_rate: Decimal, interval_hours: Option<Decimal>) -> FundingRate {
        FundingRate {
            dex,
            symbol: Symbol::Btc,
            current_rate,
            predicted_rate: None,
            interval_hours,
            next_funding_time_ms: None,
            index_price: None,
            mark_price: None,
            trace: MessageTrace::on_receive(),
        }
    }

    #[test]
    fn annualization_normalizes_different_intervals() {
        // 設計書の例: 1 時間ごと 0.001% と 8 時間ごと 0.008% は同じ年率になる
        let hourly = rate(Dex::Hyperliquid, dec!(0.00001), Some(dec!(1)));
        let eight_hourly = rate(Dex::Aster, dec!(0.00008), Some(dec!(8)));
        assert_eq!(hourly.annualized_pct(), eight_hourly.annualized_pct());
        // 0.00001 × 24 × 365 × 100 = 8.76%
        assert_eq!(hourly.annualized_pct(), Some(dec!(8.7600)));
    }

    #[test]
    fn rate_bps_and_intervals_per_year() {
        let hourly = rate(Dex::Hyperliquid, dec!(0.0001), Some(dec!(1)));
        assert_eq!(hourly.rate_bps(), dec!(1));
        assert_eq!(hourly.intervals_per_year(), Some(dec!(8760)));
        assert_eq!(hourly.annualized_bps(), Some(dec!(8760)));

        let eight = rate(Dex::Aster, dec!(0.0001), Some(dec!(8)));
        assert_eq!(eight.intervals_per_year(), Some(dec!(1095)));
    }

    #[test]
    fn unknown_interval_yields_no_annualization() {
        let unknown = rate(Dex::EdgeX, dec!(0.0001), None);
        assert_eq!(unknown.intervals_per_year(), None);
        assert_eq!(unknown.annualized_pct(), None);
        assert_eq!(unknown.annualized_bps(), None);

        let zero = rate(Dex::EdgeX, dec!(0.0001), Some(Decimal::ZERO));
        assert_eq!(zero.annualized_pct(), None);
    }

    #[test]
    fn unknown_interval_is_never_treated_as_matching() {
        let known = rate(Dex::Hyperliquid, dec!(0.0001), Some(dec!(1)));
        let unknown = rate(Dex::Lighter, dec!(0.0002), None);
        assert!(!known.same_interval_as(&unknown));
        assert!(!unknown.same_interval_as(&unknown));

        let spread = FundingSpread::between(&known, &unknown).unwrap();
        assert!(!spread.same_interval, "不明な間隔を同一扱いしない");
        assert_eq!(spread.interval_hours, None);
        assert_eq!(spread.annualized_diff_bps, None);
    }

    #[test]
    fn spread_picks_short_side_as_the_higher_rate() {
        let hl = rate(Dex::Hyperliquid, dec!(0.0003), Some(dec!(1)));
        let lighter = rate(Dex::Lighter, dec!(0.0001), Some(dec!(1)));

        let spread = FundingSpread::between(&hl, &lighter).unwrap();
        assert_eq!(spread.short_dex, Dex::Hyperliquid);
        assert_eq!(spread.long_dex, Dex::Lighter);
        assert_eq!(spread.rate_diff_bps, dec!(2));
        assert!(spread.same_interval);
        assert_eq!(spread.interval_hours, Some(dec!(1)));

        // 引数の順序を入れ替えても同じ向き
        let flipped = FundingSpread::between(&lighter, &hl).unwrap();
        assert_eq!(flipped.short_dex, Dex::Hyperliquid);
        assert_eq!(flipped.rate_diff_bps, dec!(2));
    }

    #[test]
    fn spread_handles_negative_rates() {
        let hl = rate(Dex::Hyperliquid, dec!(0.0002), Some(dec!(1)));
        let lighter = rate(Dex::Lighter, dec!(-0.0001), Some(dec!(1)));
        let spread = FundingSpread::between(&hl, &lighter).unwrap();
        assert_eq!(spread.short_dex, Dex::Hyperliquid);
        assert_eq!(spread.rate_diff_bps, dec!(3));
    }

    #[test]
    fn mismatched_intervals_are_flagged_with_annualized_comparison() {
        // 1 時間 1bps vs 8 時間 4bps。1 精算あたりは 8 時間側が高いが、年率は逆
        let hourly = rate(Dex::Hyperliquid, dec!(0.0001), Some(dec!(1)));
        let eight = rate(Dex::Aster, dec!(0.0004), Some(dec!(8)));

        let spread = FundingSpread::between(&hourly, &eight).unwrap();
        assert!(!spread.same_interval);
        assert_eq!(
            spread.short_dex,
            Dex::Aster,
            "1 精算あたりでは 8 時間側が高い"
        );
        assert_eq!(spread.rate_diff_bps, dec!(3));
        // 年率では 8760 vs 4380 → 1 時間側が高い。単純比較の危うさが数値で出る
        assert_eq!(spread.annualized_diff_bps, Some(dec!(-4380)));
    }

    #[test]
    fn spread_requires_two_distinct_dexes_and_a_difference() {
        let hl = rate(Dex::Hyperliquid, dec!(0.0001), Some(dec!(1)));
        let same = rate(Dex::Lighter, dec!(0.0001), Some(dec!(1)));
        assert!(FundingSpread::between(&hl, &same).is_none());
        assert!(FundingSpread::between(&hl, &hl).is_none());

        let other_symbol = FundingRate {
            symbol: Symbol::Eth,
            ..rate(Dex::Lighter, dec!(0.0005), Some(dec!(1)))
        };
        assert!(FundingSpread::between(&hl, &other_symbol).is_none());
    }

    #[test]
    fn breakeven_intervals_rounds_up() {
        assert_eq!(breakeven_intervals(dec!(3), dec!(10)), Some(4));
        assert_eq!(breakeven_intervals(dec!(5), dec!(10)), Some(2));
        assert_eq!(breakeven_intervals(dec!(20), dec!(10)), Some(1));
        assert_eq!(breakeven_intervals(dec!(3), dec!(0)), Some(0));
        assert_eq!(breakeven_intervals(dec!(0), dec!(10)), None);
        assert_eq!(breakeven_intervals(dec!(-1), dec!(10)), None);
    }
}
