//! ファンディングレートの集約。
//!
//! 板と同じく「(DEX × 銘柄) の最新のみ」を保持する。24 時間稼働でもキー数は
//! 固定で、レートは毎回上書きされる。
//!
//! 収集は `dex-*` crate の [`FundingRateSource`] 実装が担当する（Hyperliquid の
//! `activeAssetCtx` / Lighter の `market_stats`）。フェーズ1 では収集した
//! レートを CSV に残すのが主目的で、この store は戦略判定
//! （`strategy-funding-arb`）が参照する。
//!
//! [`FundingRateSource`]: https://docs.rs/dex-traits

use core_types::{Dex, FundingRate, FundingSpread, Symbol};
use dashmap::DashMap;

/// 銘柄ごとに全 DEX の最新ファンディングレートを保持する共有状態。
#[derive(Debug, Default)]
pub struct FundingStore {
    rates: DashMap<(Dex, Symbol), FundingRate>,
}

impl FundingStore {
    pub fn new() -> Self {
        FundingStore {
            rates: DashMap::new(),
        }
    }

    pub fn update(&self, rate: FundingRate) {
        self.rates.insert((rate.dex, rate.symbol), rate);
    }

    pub fn get(&self, dex: Dex, symbol: Symbol) -> Option<FundingRate> {
        self.rates.get(&(dex, symbol)).map(|r| *r)
    }

    pub fn len(&self) -> usize {
        self.rates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rates.is_empty()
    }

    /// 2 DEX 間のレート差。どちらかが未受信なら `None`。
    pub fn spread(&self, symbol: Symbol, dex_a: Dex, dex_b: Dex) -> Option<FundingSpread> {
        let a = self.rates.get(&(dex_a, symbol))?;
        let b = self.rates.get(&(dex_b, symbol))?;
        FundingSpread::between(&a, &b)
    }

    /// 複数ペアのレート差をまとめて求める。
    ///
    /// レートが揃っていないペア（= その銘柄がその DEX に無い、まだ未受信）は
    /// 板と同じく**正常系としてスキップ**する。
    pub fn spreads(&self, symbol: Symbol, pairs: &[(Dex, Dex)]) -> Vec<FundingSpread> {
        pairs
            .iter()
            .filter_map(|(a, b)| self.spread(symbol, *a, *b))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::MessageTrace;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    fn rate(dex: Dex, symbol: Symbol, current_rate: Decimal) -> FundingRate {
        FundingRate {
            dex,
            symbol,
            current_rate,
            predicted_rate: None,
            interval_hours: Some(dec!(1)),
            next_funding_time_ms: None,
            index_price: None,
            mark_price: None,
            open_interest: None,
            volume_24h_usd: None,
            trace: MessageTrace::on_receive(),
        }
    }

    #[test]
    fn update_overwrites_same_key() {
        let store = FundingStore::new();
        store.update(rate(Dex::Hyperliquid, Symbol::Btc, dec!(0.0001)));
        store.update(rate(Dex::Hyperliquid, Symbol::Btc, dec!(0.0005)));
        assert_eq!(store.len(), 1);
        assert_eq!(
            store
                .get(Dex::Hyperliquid, Symbol::Btc)
                .unwrap()
                .current_rate,
            dec!(0.0005)
        );
    }

    #[test]
    fn spread_requires_both_sides() {
        let store = FundingStore::new();
        store.update(rate(Dex::Hyperliquid, Symbol::Btc, dec!(0.0003)));
        assert!(store
            .spread(Symbol::Btc, Dex::Hyperliquid, Dex::Lighter)
            .is_none());

        store.update(rate(Dex::Lighter, Symbol::Btc, dec!(0.0001)));
        let spread = store
            .spread(Symbol::Btc, Dex::Hyperliquid, Dex::Lighter)
            .unwrap();
        assert_eq!(spread.short_dex, Dex::Hyperliquid);
        assert_eq!(spread.rate_diff_bps, dec!(2));
    }

    #[test]
    fn spreads_skip_pairs_without_data() {
        let store = FundingStore::new();
        store.update(rate(Dex::Hyperliquid, Symbol::Btc, dec!(0.0003)));
        store.update(rate(Dex::Lighter, Symbol::Btc, dec!(0.0001)));

        let pairs = crate::pairs::dex_pairs(&Dex::ALL);
        assert!(pairs.len() > 1);
        // データがあるのは 1 ペアだけ
        assert_eq!(store.spreads(Symbol::Btc, &pairs).len(), 1);
    }
}
