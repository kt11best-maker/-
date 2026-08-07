//! 手数料表。
//!
//! **手数料率は必ず設定ファイルから与えること。** 既定値を持たせると、確認前の
//! 値で利益判定が通ってしまう。未設定の DEX を含むペアは、戦略側でシグナルを
//! 出さずにスキップする（[`FeeSchedule::get`] が `None` を返す）。

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::symbol::Dex;

/// 1 DEX の手数料率（bps）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DexFees {
    /// テイカー料率（bps）。IOC 執行で使う。
    pub taker_bps: Decimal,
    /// メイカー料率（bps）。指値執行で使う。リベートならマイナス。
    pub maker_bps: Decimal,
}

/// 執行方式。手数料の計算に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionStyle {
    /// IOC 等のテイカー執行。
    Taker,
    /// 指値（メイカー）執行。約定しない可能性がある点は別途考慮すること。
    Maker,
}

/// DEX ごとの手数料表。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeeSchedule {
    fees: BTreeMap<Dex, DexFees>,
}

impl FeeSchedule {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_map(fees: BTreeMap<Dex, DexFees>) -> Self {
        FeeSchedule { fees }
    }

    pub fn insert(&mut self, dex: Dex, fees: DexFees) {
        self.fees.insert(dex, fees);
    }

    /// 未設定の DEX は `None`。呼び出し側はシグナルを出さないこと。
    pub fn get(&self, dex: Dex) -> Option<DexFees> {
        self.fees.get(&dex).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.fees.is_empty()
    }

    /// 設定されていない DEX を返す（起動時の警告用）。
    pub fn missing<'a>(&'a self, dexes: &'a [Dex]) -> Vec<Dex> {
        dexes
            .iter()
            .copied()
            .filter(|d| !self.fees.contains_key(d))
            .collect()
    }

    fn rate(&self, dex: Dex, style: ExecutionStyle) -> Option<Decimal> {
        let fees = self.get(dex)?;
        Some(match style {
            ExecutionStyle::Taker => fees.taker_bps,
            ExecutionStyle::Maker => fees.maker_bps,
        })
    }

    /// 両建てを**建てるだけ**の手数料（2 DEX × 1 回 = 2 レグ）。
    pub fn entry_bps(
        &self,
        long_dex: Dex,
        short_dex: Dex,
        style: ExecutionStyle,
    ) -> Option<Decimal> {
        Some(self.rate(long_dex, style)? + self.rate(short_dex, style)?)
    }

    /// 両建てを**建てて決済するまで**の手数料（2 DEX × 建て/決済 = 4 レグ）。
    ///
    /// 決済は約定を優先するためテイカーになりがちなので、建てと決済で執行方式を
    /// 分けて指定できるようにしている。
    pub fn round_trip_bps(
        &self,
        long_dex: Dex,
        short_dex: Dex,
        entry_style: ExecutionStyle,
        exit_style: ExecutionStyle,
    ) -> Option<Decimal> {
        Some(
            self.entry_bps(long_dex, short_dex, entry_style)?
                + self.entry_bps(long_dex, short_dex, exit_style)?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn schedule() -> FeeSchedule {
        let mut s = FeeSchedule::new();
        s.insert(
            Dex::Hyperliquid,
            DexFees {
                taker_bps: dec!(4.5),
                maker_bps: dec!(1.5),
            },
        );
        s.insert(
            Dex::Lighter,
            DexFees {
                taker_bps: dec!(2.0),
                // リベート（受け取り）はマイナスで表す
                maker_bps: dec!(-0.5),
            },
        );
        s
    }

    #[test]
    fn entry_and_round_trip() {
        let s = schedule();
        // 建てのみ: 4.5 + 2.0
        assert_eq!(
            s.entry_bps(Dex::Hyperliquid, Dex::Lighter, ExecutionStyle::Taker),
            Some(dec!(6.5))
        );
        // 建て + 決済 = 4 レグ
        assert_eq!(
            s.round_trip_bps(
                Dex::Hyperliquid,
                Dex::Lighter,
                ExecutionStyle::Taker,
                ExecutionStyle::Taker
            ),
            Some(dec!(13.0))
        );
        // 指値で建てて成行で決済する場合
        assert_eq!(
            s.round_trip_bps(
                Dex::Hyperliquid,
                Dex::Lighter,
                ExecutionStyle::Maker,
                ExecutionStyle::Taker
            ),
            Some(dec!(7.5))
        );
    }

    #[test]
    fn unknown_dex_yields_none_so_strategies_skip_it() {
        let s = schedule();
        assert_eq!(s.get(Dex::Aster), None);
        assert_eq!(
            s.entry_bps(Dex::Hyperliquid, Dex::Aster, ExecutionStyle::Taker),
            None
        );
        assert_eq!(
            s.missing(&Dex::ALL),
            vec![Dex::EdgeX, Dex::Aster, Dex::Dydx]
        );
    }

    #[test]
    fn empty_schedule_reports_everything_missing() {
        let s = FeeSchedule::new();
        assert!(s.is_empty());
        assert_eq!(s.missing(&Dex::ALL).len(), Dex::ALL.len());
    }
}
