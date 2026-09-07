//! 実ポジションの取得インターフェースと、bot の想定ポジション。
//!
//! # 監視対象は「実ポジション」であること（最重要）
//!
//! ネットデルタの監視で見るのは、**各 DEX の API から取得した実ポジション**で
//! あって、bot が記録している想定ポジションではない。想定と実際が乖離している
//! こと自体が検知すべき異常だからである（約定通知の取りこぼし、強制決済・ADL、
//! クラッシュ後の復元漏れ）。
//!
//! # フェーズ1 時点の位置づけ
//!
//! 実ポジション取得には認証付き API が必要で、発注機能の無いフェーズ1 では
//! 実装できない。ここには [`PositionSource`] の**インターフェースだけ**を用意し、
//! フェーズ3 で各 DEX の実装を足して有効化する。判定ロジック
//! （[`crate::net_delta`]）は I/O を持たない純粋関数なので、実装が入る前でも
//! テストで検証できる。

use std::collections::HashMap;

use async_trait::async_trait;
use core_types::{Dex, Quantity, Symbol};
use rust_decimal::Decimal;

/// 1 DEX × 1 銘柄の実ポジション。
///
/// **符号の規約: ロングが正、ショートが負。** 各 DEX の API がどの表現を返して
/// いても、[`PositionSource`] の実装側でこの規約に正規化すること。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DexPosition {
    pub dex: Dex,
    pub symbol: Symbol,
    pub quantity: Quantity,
}

impl DexPosition {
    pub fn new(dex: Dex, symbol: Symbol, quantity: Quantity) -> Self {
        DexPosition {
            dex,
            symbol,
            quantity,
        }
    }

    pub fn is_flat(&self) -> bool {
        self.quantity.raw().is_zero()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PositionError {
    #[error("ポジション取得 API が失敗: {0}")]
    Api(String),

    #[error("認証に失敗: {0}")]
    Auth(String),

    #[error("レスポンスのパースに失敗: {0}")]
    Parse(String),

    #[error("この DEX はポジション取得に対応していません")]
    Unsupported,
}

/// 各 DEX の実ポジションを取得する。
///
/// 実装は「その銘柄のポジションが無い = 数量 0」を返してもよいし、省略しても
/// よい（呼び出し側が 0 として扱う）。**ただし取得に失敗した場合は必ず `Err` を
/// 返すこと。** 失敗を「空 = フラット」と区別できないと、片肺ポジションを
/// 見落とす。
#[async_trait]
pub trait PositionSource: Send + Sync {
    fn dex(&self) -> Dex;

    /// 指定銘柄の実ポジションを取得する。
    async fn positions(&self, symbols: &[Symbol]) -> Result<Vec<DexPosition>, PositionError>;
}

/// bot が「持っているつもり」の数量（DEX × 銘柄）。
///
/// 実ポジションとの差（drift）を取るためだけに持つ。**これ自体を監視対象に
/// してはいけない。**
#[derive(Debug, Clone, Default)]
pub struct ExpectedPositions {
    inner: HashMap<(Dex, Symbol), Quantity>,
}

impl ExpectedPositions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, dex: Dex, symbol: Symbol, quantity: Quantity) {
        if quantity.raw().is_zero() {
            self.inner.remove(&(dex, symbol));
        } else {
            self.inner.insert((dex, symbol), quantity);
        }
    }

    /// 記録が無い組み合わせは「フラット（0）」として扱う。
    pub fn get(&self, dex: Dex, symbol: Symbol) -> Quantity {
        self.inner
            .get(&(dex, symbol))
            .copied()
            .unwrap_or(Quantity::ZERO)
    }

    /// 想定を持っている銘柄（監視対象の絞り込みに使う）。
    pub fn symbols(&self) -> Vec<Symbol> {
        let mut symbols: Vec<Symbol> = self.inner.keys().map(|(_, s)| *s).collect();
        symbols.sort();
        symbols.dedup();
        symbols
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn clear(&mut self) {
        self.inner.clear();
    }

    /// 想定ポジションの合計（銘柄単位）。参考値。
    pub fn net(&self, symbol: Symbol) -> Quantity {
        Quantity(
            self.inner
                .iter()
                .filter(|((_, s), _)| *s == symbol)
                .map(|(_, q)| q.raw())
                .sum::<Decimal>(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn unknown_pairs_are_flat() {
        let expected = ExpectedPositions::new();
        assert_eq!(
            expected.get(Dex::Hyperliquid, Symbol::Btc),
            Quantity::ZERO,
            "記録が無い = フラット"
        );
        assert!(expected.is_empty());
    }

    #[test]
    fn setting_zero_removes_the_entry() {
        let mut expected = ExpectedPositions::new();
        expected.set(Dex::Lighter, Symbol::Btc, Quantity(dec!(-0.5)));
        assert_eq!(expected.symbols(), vec![Symbol::Btc]);

        expected.set(Dex::Lighter, Symbol::Btc, Quantity::ZERO);
        assert!(expected.is_empty(), "決済したら記録も消える");
    }

    #[test]
    fn net_sums_across_dexes() {
        let mut expected = ExpectedPositions::new();
        expected.set(Dex::Hyperliquid, Symbol::Btc, Quantity(dec!(0.5)));
        expected.set(Dex::Lighter, Symbol::Btc, Quantity(dec!(-0.5)));
        expected.set(Dex::Lighter, Symbol::Eth, Quantity(dec!(2)));

        assert_eq!(expected.net(Symbol::Btc), Quantity::ZERO);
        assert_eq!(expected.net(Symbol::Eth), Quantity(dec!(2)));
        assert_eq!(expected.symbols(), vec![Symbol::Btc, Symbol::Eth]);
    }
}
