//! 流動性指標（Open Interest / 取引量）。
//!
//! # 何に使うか
//!
//! - **見かけの流動性と実需の乖離**: 出来高に対して OI が極端に小さい場合、
//!   ポイント稼ぎ目的の回転売買が出来高を膨らませている可能性がある。板が想定より
//!   薄く、アービトラージの執行に耐えないことを示唆する。
//! - **ファンディングレートの背景理解**: OI が偏っている DEX はレートが高くなる。
//!   レート差が持続するかの判断材料になる。
//! - **銘柄の選定**: OI が小さすぎる銘柄は、想定サイズが板を動かしてしまう。
//!
//! # 単位を混ぜないこと（最重要）
//!
//! `open_interest` は**契約数量**、`volume_24h_usd` は **USD 建て**。
//! そのまま割ると意味を持たないため、[`MarketStats::volume_oi_ratio`] は OI を
//! [`MarketStats::reference_price`] でノーショナル換算してから割る。価格が無ければ
//! 比率は `None` になる（推測で埋めない）。
//!
//! # 収集経路
//!
//! **新しい WS 接続は増やさない。** Hyperliquid の `activeAssetCtx` / Lighter の
//! `market_stats` はどちらもファンディングと同じメッセージに含まれるため、
//! [`FundingRate`](crate::FundingRate) に相乗りして流れてくる
//! （[`FundingRate::market_stats`] で取り出す）。

use rust_decimal::Decimal;

use crate::num::{Price, Quantity};
use crate::symbol::{Dex, Symbol};
use crate::trace::MessageTrace;

/// 1 DEX × 1 銘柄の流動性指標。
#[derive(Debug, Clone, Copy)]
pub struct MarketStats {
    pub dex: Dex,
    pub symbol: Symbol,
    /// 未決済建玉（**契約数量**。USD ではない）。
    pub open_interest: Option<Quantity>,
    /// 直近 24 時間の取引量（**USD 建て**）。
    pub volume_24h_usd: Option<Decimal>,
    /// OI をノーショナル換算するための価格（マーク価格を優先し、無ければ
    /// インデックス価格）。**無ければ換算しない。**
    pub reference_price: Option<Price>,
    pub trace: MessageTrace,
}

impl MarketStats {
    /// 指標が 1 つでも入っているか。両方 `None` なら記録する価値がない。
    pub fn has_any(&self) -> bool {
        self.open_interest.is_some() || self.volume_24h_usd.is_some()
    }

    /// ノーショナル換算した OI（USD）。価格が無ければ `None`。
    pub fn open_interest_usd(&self) -> Option<Decimal> {
        let oi = self.open_interest?;
        let price = self.reference_price?;
        if price.raw() <= Decimal::ZERO {
            return None;
        }
        Some(oi.raw() * price.raw())
    }

    /// 出来高 / OI（どちらも USD 換算した上で割る）。
    ///
    /// **高すぎる場合は回転売買の疑い。** OI がゼロ、または価格が無くて換算
    /// できない場合は `None`。
    pub fn volume_oi_ratio(&self) -> Option<Decimal> {
        let volume = self.volume_24h_usd?;
        let oi_usd = self.open_interest_usd()?;
        if oi_usd <= Decimal::ZERO {
            return None;
        }
        Some(volume / oi_usd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn stats(oi: Option<Decimal>, volume: Option<Decimal>, price: Option<Decimal>) -> MarketStats {
        MarketStats {
            dex: Dex::Hyperliquid,
            symbol: Symbol::Btc,
            open_interest: oi.map(Quantity),
            volume_24h_usd: volume,
            reference_price: price.map(Price),
            trace: MessageTrace::on_receive(),
        }
    }

    #[test]
    fn converts_open_interest_to_notional() {
        let s = stats(Some(dec!(1000)), None, Some(dec!(36000)));
        assert_eq!(s.open_interest_usd(), Some(dec!(36000000)));
    }

    #[test]
    fn ratio_compares_usd_with_usd() {
        // OI 1000 BTC × 36,000 = 36,000,000 USD、出来高 72,000,000 USD → 2.0
        let s = stats(Some(dec!(1000)), Some(dec!(72000000)), Some(dec!(36000)));
        assert_eq!(s.volume_oi_ratio(), Some(dec!(2)));
    }

    #[test]
    fn ratio_needs_a_price_to_normalize_units() {
        // 価格が無ければ「数量 vs USD」を割ることになるので計算しない
        let s = stats(Some(dec!(1000)), Some(dec!(72000000)), None);
        assert_eq!(s.open_interest_usd(), None);
        assert_eq!(s.volume_oi_ratio(), None);
    }

    #[test]
    fn zero_open_interest_yields_no_ratio() {
        let s = stats(Some(Decimal::ZERO), Some(dec!(1000)), Some(dec!(36000)));
        assert_eq!(s.volume_oi_ratio(), None);
    }

    #[test]
    fn missing_metrics_are_reported() {
        assert!(!stats(None, None, Some(dec!(36000))).has_any());
        assert!(stats(Some(dec!(1)), None, None).has_any());
        assert!(stats(None, Some(dec!(1)), None).has_any());
    }
}
