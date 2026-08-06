//! ファンディングレートの共通型。
//!
//! # 精算間隔の扱い
//!
//! DEX ごとに精算間隔が違う（Hyperliquid / Lighter は 1 時間、Aster は 8 時間）。
//! **`interval_hours` を必ず参照すること。** 間隔が異なる DEX 同士のレートを
//! そのまま引き算してはいけない。異なる間隔を比較する場合は
//! [`FundingRate::annualized_bps`] で正規化する。
//!
//! # 符号の規約
//!
//! `rate > 0` = **ロングがショートに支払う**、を前提とする。DEX ごとに規約が
//! 違う場合は、各 dex crate の取り込み時点でこの規約に正規化すること
//! （フェーズ1 の収集で一次情報を確認する項目）。

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::num::BPS_DENOMINATOR;
use crate::symbol::{Dex, Symbol};
use crate::trace::MessageTrace;

/// 1 年の時間数（年率換算に使う）。
const HOURS_PER_YEAR: Decimal = Decimal::from_parts(8_760, 0, 0, false, 0);

/// 1 精算あたりのファンディングレート。
#[derive(Debug, Clone, Copy)]
pub struct FundingRate {
    pub dex: Dex,
    pub symbol: Symbol,
    /// 1 精算あたりのレート（比率）。`0.0001` = 1bps。
    /// 正ならロングが支払う。
    pub rate: Decimal,
    /// 精算間隔（時間）。Hyperliquid / Lighter = 1、Aster = 8。
    pub interval_hours: Decimal,
    /// 次回精算時刻（wall clock, ms epoch）。API が提供しない場合は `None`。
    ///
    /// 両 DEX の精算タイミングがずれる場合、両方の精算を跨ぐのに必要な待ち時間を
    /// 見積もるために使う。
    pub next_funding_time_ms: Option<u64>,
    pub trace: MessageTrace,
}

impl FundingRate {
    /// 1 精算あたりのレート（bps）。
    pub fn rate_bps(&self) -> Decimal {
        self.rate * BPS_DENOMINATOR
    }

    /// 年間の精算回数。
    pub fn intervals_per_year(&self) -> Option<Decimal> {
        if self.interval_hours <= Decimal::ZERO {
            return None;
        }
        Some(HOURS_PER_YEAR / self.interval_hours)
    }

    /// 年率換算したレート（bps）。精算間隔が異なる DEX 同士の比較に使う。
    pub fn annualized_bps(&self) -> Option<Decimal> {
        Some(self.rate_bps() * self.intervals_per_year()?)
    }

    /// 年率換算したレート（%）。
    pub fn annualized_pct(&self) -> Option<Decimal> {
        Some(self.annualized_bps()? / Decimal::ONE_HUNDRED)
    }

    /// 精算間隔が同じか。
    pub fn same_interval_as(&self, other: &FundingRate) -> bool {
        self.interval_hours == other.interval_hours
    }

    /// データの鮮度（現在時刻との差, ms）。
    pub fn age_ms(&self, now_wall_ms: u64) -> i64 {
        now_wall_ms as i64 - self.trace.received_wall_ms as i64
    }
}

/// 2 DEX 間のファンディングレート差。
///
/// 「レートが高い DEX でショート、低い DEX でロング」が裁定の建て方。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FundingSpread {
    pub symbol: Symbol,
    /// ロングを建てる側（レートが低い = 受け取る、または支払いが少ない）。
    pub long_dex: Dex,
    /// ショートを建てる側（レートが高い = 受け取る）。
    pub short_dex: Dex,
    /// 1 精算あたりのレート差（bps, 非負）。
    ///
    /// 精算間隔が同じ場合のみ意味を持つ。異なる場合は
    /// [`FundingSpread::annualized_diff_bps`] を使うこと。
    pub rate_diff_bps: Decimal,
    /// 年率換算のレート差（bps, 非負）。間隔が違う DEX 同士でも比較できる。
    pub annualized_diff_bps: Option<Decimal>,
    /// 両 DEX の精算間隔が同じか。異なる場合、`rate_diff_bps` を
    /// そのまま「1 精算あたりの収益」として扱ってはいけない。
    pub same_interval: bool,
    /// 精算間隔（時間）。同じ場合のみ `Some`。
    pub interval_hours: Option<Decimal>,
}

impl FundingSpread {
    /// 2 つのレートから裁定の向きと差を求める。
    ///
    /// 銘柄が違う / 同じ DEX 同士の場合は `None`。レート差が 0 の場合も
    /// 向きが決まらないため `None`。
    pub fn between(a: &FundingRate, b: &FundingRate) -> Option<FundingSpread> {
        if a.symbol != b.symbol || a.dex == b.dex {
            return None;
        }
        if a.rate == b.rate {
            return None;
        }

        // レートが高い方でショート（受け取る側）、低い方でロング
        let (short, long) = if a.rate > b.rate { (a, b) } else { (b, a) };

        let same_interval = a.same_interval_as(b);
        let rate_diff_bps = short.rate_bps() - long.rate_bps();
        let annualized_diff_bps = match (short.annualized_bps(), long.annualized_bps()) {
            (Some(s), Some(l)) => Some(s - l),
            _ => None,
        };

        Some(FundingSpread {
            symbol: a.symbol,
            long_dex: long.dex,
            short_dex: short.dex,
            rate_diff_bps,
            annualized_diff_bps,
            same_interval,
            interval_hours: same_interval.then_some(a.interval_hours),
        })
    }

    /// 手数料を回収するのに必要な最低精算回数。
    ///
    /// レート差が 0 以下なら `None`（回収不能）。
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

    #[test]
    fn rate_bps_and_annualization() {
        // 1 時間精算で 0.01% = 1bps → 年 8760 回
        let hourly = rate(Dex::Hyperliquid, dec!(0.0001), dec!(1));
        assert_eq!(hourly.rate_bps(), dec!(1));
        assert_eq!(hourly.intervals_per_year(), Some(dec!(8760)));
        assert_eq!(hourly.annualized_bps(), Some(dec!(8760)));
        assert_eq!(hourly.annualized_pct(), Some(dec!(87.60)));

        // 8 時間精算で同じレートなら年率は 1/8
        let eight_hourly = rate(Dex::Aster, dec!(0.0001), dec!(8));
        assert_eq!(eight_hourly.annualized_bps(), Some(dec!(1095)));
    }

    #[test]
    fn annualization_rejects_zero_interval() {
        let broken = rate(Dex::Hyperliquid, dec!(0.0001), dec!(0));
        assert_eq!(broken.intervals_per_year(), None);
        assert_eq!(broken.annualized_bps(), None);
    }

    #[test]
    fn spread_picks_short_side_as_the_higher_rate() {
        // Hyperliquid の方がレートが高い → Hyperliquid でショート
        let hl = rate(Dex::Hyperliquid, dec!(0.0003), dec!(1));
        let lighter = rate(Dex::Lighter, dec!(0.0001), dec!(1));

        let spread = FundingSpread::between(&hl, &lighter).unwrap();
        assert_eq!(spread.short_dex, Dex::Hyperliquid);
        assert_eq!(spread.long_dex, Dex::Lighter);
        assert_eq!(spread.rate_diff_bps, dec!(2));
        assert!(spread.same_interval);
        assert_eq!(spread.interval_hours, Some(dec!(1)));

        // 引数の順序を入れ替えても同じ向きになる
        let flipped = FundingSpread::between(&lighter, &hl).unwrap();
        assert_eq!(flipped.short_dex, Dex::Hyperliquid);
        assert_eq!(flipped.rate_diff_bps, dec!(2));
    }

    #[test]
    fn spread_handles_negative_rates() {
        // マイナスのレート = ショートがロングに支払う。低い方でロングを建てる
        let hl = rate(Dex::Hyperliquid, dec!(0.0002), dec!(1));
        let lighter = rate(Dex::Lighter, dec!(-0.0001), dec!(1));

        let spread = FundingSpread::between(&hl, &lighter).unwrap();
        assert_eq!(spread.short_dex, Dex::Hyperliquid);
        assert_eq!(spread.long_dex, Dex::Lighter);
        assert_eq!(spread.rate_diff_bps, dec!(3));
    }

    #[test]
    fn spread_flags_mismatched_intervals() {
        // 1 時間精算 1bps vs 8 時間精算 4bps。
        // 1 精算あたりでは 8 時間側が高いが、年率では 1 時間側が高い。
        let hourly = rate(Dex::Hyperliquid, dec!(0.0001), dec!(1));
        let eight = rate(Dex::Aster, dec!(0.0004), dec!(8));

        let spread = FundingSpread::between(&hourly, &eight).unwrap();
        assert!(!spread.same_interval, "間隔が違うことを明示する");
        assert_eq!(spread.interval_hours, None);
        // 1 精算あたりの差は 8 時間側が高い方向
        assert_eq!(spread.short_dex, Dex::Aster);
        assert_eq!(spread.rate_diff_bps, dec!(3));
        // 年率だと 8760 vs 4380 で 1 時間側が高い → 単純比較の危うさが数値で出る
        assert_eq!(spread.annualized_diff_bps, Some(dec!(-4380)));
    }

    #[test]
    fn spread_requires_two_distinct_dexes_and_a_difference() {
        let hl = rate(Dex::Hyperliquid, dec!(0.0001), dec!(1));
        let same = rate(Dex::Lighter, dec!(0.0001), dec!(1));
        assert!(FundingSpread::between(&hl, &same).is_none(), "差が無い");
        assert!(FundingSpread::between(&hl, &hl).is_none(), "同一 DEX");

        let other_symbol = FundingRate {
            symbol: Symbol::Eth,
            ..rate(Dex::Lighter, dec!(0.0005), dec!(1))
        };
        assert!(FundingSpread::between(&hl, &other_symbol).is_none());
    }

    #[test]
    fn breakeven_intervals_rounds_up() {
        // 手数料 10bps、1 精算 3bps → 4 回必要（3×3=9 では足りない）
        assert_eq!(breakeven_intervals(dec!(3), dec!(10)), Some(4));
        // ちょうど割り切れる場合
        assert_eq!(breakeven_intervals(dec!(5), dec!(10)), Some(2));
        // レート差が手数料を 1 回で上回る
        assert_eq!(breakeven_intervals(dec!(20), dec!(10)), Some(1));
        // 手数料ゼロなら 0 回
        assert_eq!(breakeven_intervals(dec!(3), dec!(0)), Some(0));
        // レート差が無ければ回収不能
        assert_eq!(breakeven_intervals(dec!(0), dec!(10)), None);
        assert_eq!(breakeven_intervals(dec!(-1), dec!(10)), None);
    }

    #[test]
    fn breakeven_via_spread() {
        let hl = rate(Dex::Hyperliquid, dec!(0.0003), dec!(1));
        let lighter = rate(Dex::Lighter, dec!(0.0001), dec!(1));
        let spread = FundingSpread::between(&hl, &lighter).unwrap();
        // レート差 2bps、手数料 9bps → 5 回
        assert_eq!(spread.breakeven_intervals(dec!(9)), Some(5));
    }
}
