use std::fmt;
use std::ops::{Add, Div, Mul, Sub};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// bps 換算の分母（1 bps = 1/10000）。
pub const BPS_DENOMINATOR: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);

/// 価格。[`Quantity`] との取り違えを防ぐため newtype で包む。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Price(pub Decimal);

/// 数量。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Quantity(pub Decimal);

impl Price {
    pub const ZERO: Price = Price(Decimal::ZERO);

    #[inline]
    pub fn new(v: Decimal) -> Self {
        Price(v)
    }

    #[inline]
    pub fn raw(&self) -> Decimal {
        self.0
    }

    #[inline]
    pub fn is_positive(&self) -> bool {
        self.0 > Decimal::ZERO
    }

    /// `self` を基準としたときの `other` との乖離（bps）。
    ///
    /// 分母には両者の中点を使い、A/B のどちらを基準にしても符号が反転するだけで
    /// 絶対値が変わらないようにしている（非対称な基準による解析バイアスを避ける）。
    pub fn spread_bps(&self, other: Price) -> Option<Decimal> {
        let reference = (self.0 + other.0) / Decimal::TWO;
        if reference.is_zero() {
            return None;
        }
        Some((self.0 - other.0) / reference * BPS_DENOMINATOR)
    }
}

impl Quantity {
    pub const ZERO: Quantity = Quantity(Decimal::ZERO);

    #[inline]
    pub fn new(v: Decimal) -> Self {
        Quantity(v)
    }

    #[inline]
    pub fn raw(&self) -> Decimal {
        self.0
    }

    #[inline]
    pub fn is_positive(&self) -> bool {
        self.0 > Decimal::ZERO
    }
}

macro_rules! impl_decimal_ops {
    ($t:ty) => {
        impl Add for $t {
            type Output = $t;
            fn add(self, rhs: $t) -> $t {
                Self(self.0 + rhs.0)
            }
        }
        impl Sub for $t {
            type Output = $t;
            fn sub(self, rhs: $t) -> $t {
                Self(self.0 - rhs.0)
            }
        }
        impl Mul<Decimal> for $t {
            type Output = $t;
            fn mul(self, rhs: Decimal) -> $t {
                Self(self.0 * rhs)
            }
        }
        impl Div<Decimal> for $t {
            type Output = $t;
            fn div(self, rhs: Decimal) -> $t {
                Self(self.0 / rhs)
            }
        }
        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

impl_decimal_ops!(Price);
impl_decimal_ops!(Quantity);

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn bps_denominator_is_ten_thousand() {
        assert_eq!(BPS_DENOMINATOR, dec!(10000));
    }

    #[test]
    fn spread_bps_is_antisymmetric() {
        let a = Price(dec!(100.5));
        let b = Price(dec!(100.0));
        let ab = a.spread_bps(b).unwrap();
        let ba = b.spread_bps(a).unwrap();
        assert_eq!(ab, -ba);
        // (100.5 - 100) / 100.25 * 10000 ≒ 49.875 bps
        assert!((ab - dec!(49.8753)).abs() < dec!(0.001), "{ab}");
    }

    #[test]
    fn spread_bps_rejects_zero_reference() {
        assert!(Price(dec!(0)).spread_bps(Price(dec!(0))).is_none());
    }
}
