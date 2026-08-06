//! DEX ペアの列挙。
//!
//! 有効な DEX の集合は設定から動的に決まるため、ペアはここで機械的に導出する。
//! DEX を追加しても上位レイヤーのコード変更は不要。

use core_types::Dex;

/// 比較すべき全ペアを列挙する（N 個の DEX に対して N(N-1)/2 通り）。
///
/// 各ペアは `Dex` の宣言順で正規化され、戻り値もその順で安定する。CSV の
/// `dex_a`/`dex_b` の並びが実行ごとに変わらないようにするため。重複した DEX は
/// 取り除かれる。
pub fn dex_pairs(dexes: &[Dex]) -> Vec<(Dex, Dex)> {
    let mut unique: Vec<Dex> = dexes.to_vec();
    unique.sort();
    unique.dedup();

    let mut pairs = Vec::with_capacity(unique.len().saturating_sub(1) * unique.len() / 2);
    for (i, a) in unique.iter().enumerate() {
        for b in unique.iter().skip(i + 1) {
            pairs.push((*a, *b));
        }
    }
    pairs
}

/// `dex` が関与するペアのみを返す。
///
/// 板が更新されるたびに全ペアを再計算すると、更新の無かった DEX 同士の行が
/// 同じ内容で何度も出力される。イベント駆動では「更新された DEX を含むペア」
/// だけを再計算すれば十分（N=4 なら 1 更新あたり 3 ペア）。
pub fn pairs_involving(dex: Dex, dexes: &[Dex]) -> Vec<(Dex, Dex)> {
    dex_pairs(dexes)
        .into_iter()
        .filter(|(a, b)| *a == dex || *b == dex)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerates_n_choose_2() {
        assert_eq!(dex_pairs(&[]), vec![]);
        assert_eq!(dex_pairs(&[Dex::Hyperliquid]), vec![]);
        assert_eq!(
            dex_pairs(&[Dex::Hyperliquid, Dex::EdgeX]),
            vec![(Dex::Hyperliquid, Dex::EdgeX)]
        );
        // 4 DEX → 6 ペア
        assert_eq!(dex_pairs(&Dex::ALL).len(), 6);
    }

    #[test]
    fn pairs_are_canonically_ordered_regardless_of_input_order() {
        let forward = dex_pairs(&Dex::ALL);
        let mut reversed = Dex::ALL.to_vec();
        reversed.reverse();
        assert_eq!(dex_pairs(&reversed), forward);

        assert_eq!(
            forward,
            vec![
                (Dex::Hyperliquid, Dex::EdgeX),
                (Dex::Hyperliquid, Dex::Aster),
                (Dex::Hyperliquid, Dex::Lighter),
                (Dex::EdgeX, Dex::Aster),
                (Dex::EdgeX, Dex::Lighter),
                (Dex::Aster, Dex::Lighter),
            ]
        );
    }

    #[test]
    fn duplicates_are_ignored() {
        assert_eq!(
            dex_pairs(&[Dex::Aster, Dex::Aster, Dex::EdgeX]),
            vec![(Dex::EdgeX, Dex::Aster)]
        );
    }

    #[test]
    fn pairs_involving_selects_only_relevant_ones() {
        let involving = pairs_involving(Dex::Aster, &Dex::ALL);
        assert_eq!(involving.len(), 3);
        assert!(involving
            .iter()
            .all(|(a, b)| *a == Dex::Aster || *b == Dex::Aster));

        // 各 DEX の関与ペアを合計すると 全ペア × 2 になる（ペアは 2 DEX を含む）
        let total: usize = Dex::ALL
            .iter()
            .map(|d| pairs_involving(*d, &Dex::ALL).len())
            .sum();
        assert_eq!(total, dex_pairs(&Dex::ALL).len() * 2);
    }

    #[test]
    fn unknown_dex_yields_no_pairs() {
        // 有効化されていない DEX を指定しても空になる
        assert!(pairs_involving(Dex::Lighter, &[Dex::Hyperliquid, Dex::EdgeX]).is_empty());
    }
}
