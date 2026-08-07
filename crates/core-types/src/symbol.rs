use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// 対象 DEX。
///
/// 宣言順が [`Dex::ALL`] とペア列挙の正準順序になる。CSV の `dex_a`/`dex_b` は
/// 常にこの順序で並ぶため、行ごとに符号の意味が変わらない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Dex {
    Hyperliquid,
    EdgeX,
    Aster,
    Lighter,
    Dydx,
}

impl Dex {
    pub const ALL: [Dex; 5] = [
        Dex::Hyperliquid,
        Dex::EdgeX,
        Dex::Aster,
        Dex::Lighter,
        Dex::Dydx,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Dex::Hyperliquid => "hyperliquid",
            Dex::EdgeX => "edgex",
            Dex::Aster => "aster",
            Dex::Lighter => "lighter",
            Dex::Dydx => "dydx",
        }
    }

    /// 板がクロス（bid > ask）することが**構造上正常に起こる**か。
    ///
    /// dYdX v4 は中央集権的なオーダーブックを持たないため、Indexer 経由で
    /// クロスした板が観測されうる。これは異常データではないので、板を捨てずに
    /// フラグを立てて記録し、アービトラージ判定からだけ除外する。
    pub fn allows_crossed_book(&self) -> bool {
        matches!(self, Dex::Dydx)
    }
}

impl fmt::Display for Dex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Dex {
    type Err = ParseSymbolError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "hyperliquid" | "hl" => Ok(Dex::Hyperliquid),
            "edgex" => Ok(Dex::EdgeX),
            "aster" => Ok(Dex::Aster),
            "lighter" => Ok(Dex::Lighter),
            "dydx" | "dydx_v4" => Ok(Dex::Dydx),
            _ => Err(ParseSymbolError(s.to_string())),
        }
    }
}

/// 対象銘柄。
///
/// 型安全を優先して enum 固定にしている。銘柄追加時はここと
/// [`Symbol::to_dex_symbol`] を変更する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Symbol {
    Btc,
    Eth,
    Sol,
    Hype,
}

impl Symbol {
    pub const ALL: [Symbol; 4] = [Symbol::Btc, Symbol::Eth, Symbol::Sol, Symbol::Hype];

    /// 正規表記（設定ファイル・CSV・ログで使う内部表記）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Symbol::Btc => "BTC",
            Symbol::Eth => "ETH",
            Symbol::Sol => "SOL",
            Symbol::Hype => "HYPE",
        }
    }

    /// 各 DEX でのシンボル表記の違いを吸収する。
    ///
    /// - Hyperliquid: `l2Book` の `coin` フィールドに渡すコイン名（`"BTC"` 等）。
    /// - edgeX: 契約名（`"BTCUSD"` 等）。実際の購読には契約名から解決した
    ///   contractId を使うため、この文字列はメタデータ照合用のキーになる。
    /// - Aster: 取引ペア名（`"BTCUSDT"` 等）。WS のストリーム名では小文字にする
    ///   （`Symbol::to_stream_symbol` を使うこと）。
    /// - Lighter: ベースシンボル（`"BTC"` 等）。Lighter は銘柄を**数値の
    ///   market_index** で識別するため、この文字列は `market_stats` の `symbol`
    ///   と照合して market_index を動的に引くためのキーにしか使わない。
    /// - dYdX v4: マーケット ID（`"BTC-USD"` 等）。購読メッセージの `id` に
    ///   そのまま渡す。**HYPE-USD 市場が存在するかは未確認**のため、無ければ
    ///   `excluded_symbols` で外すこと（銘柄 × DEX の組み合わせが存在しない
    ///   ケースはパイプライン全体で正常系として扱う）。
    pub fn to_dex_symbol(&self, dex: Dex) -> &'static str {
        match dex {
            Dex::Hyperliquid | Dex::Lighter => self.as_str(),
            Dex::Dydx => match self {
                Symbol::Btc => "BTC-USD",
                Symbol::Eth => "ETH-USD",
                Symbol::Sol => "SOL-USD",
                Symbol::Hype => "HYPE-USD",
            },
            Dex::EdgeX => match self {
                Symbol::Btc => "BTCUSD",
                Symbol::Eth => "ETHUSD",
                Symbol::Sol => "SOLUSD",
                Symbol::Hype => "HYPEUSD",
            },
            Dex::Aster => match self {
                Symbol::Btc => "BTCUSDT",
                Symbol::Eth => "ETHUSDT",
                Symbol::Sol => "SOLUSDT",
                Symbol::Hype => "HYPEUSDT",
            },
        }
    }

    /// WS ストリーム名に使う表記（Aster は全て小文字）。
    pub fn to_stream_symbol(&self, dex: Dex) -> String {
        match dex {
            Dex::Aster => self.to_dex_symbol(dex).to_ascii_lowercase(),
            other => self.to_dex_symbol(other).to_string(),
        }
    }

    /// DEX 表記から内部表記へ復元する。未知の銘柄は `None`。
    pub fn from_dex_symbol(dex: Dex, s: &str) -> Option<Symbol> {
        let s = s.to_ascii_uppercase();
        Symbol::ALL
            .into_iter()
            .find(|sym| sym.to_dex_symbol(dex).eq_ignore_ascii_case(&s))
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Symbol {
    type Err = ParseSymbolError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let upper = s.to_ascii_uppercase();
        Symbol::ALL
            .into_iter()
            .find(|sym| sym.as_str() == upper)
            .ok_or_else(|| ParseSymbolError(s.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseSymbolError(pub String);

impl fmt::Display for ParseSymbolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "未知のシンボル/DEX 表記: {}", self.0)
    }
}

impl std::error::Error for ParseSymbolError {}

macro_rules! impl_str_serde {
    ($t:ty) => {
        impl Serialize for $t {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(d)?;
                raw.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

impl_str_serde!(Symbol);
impl_str_serde!(Dex);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dex_symbol_roundtrip() {
        for dex in Dex::ALL {
            for sym in Symbol::ALL {
                let raw = sym.to_dex_symbol(dex);
                assert_eq!(Symbol::from_dex_symbol(dex, raw), Some(sym), "{dex} {raw}");
            }
        }
    }

    #[test]
    fn from_dex_symbol_is_case_insensitive() {
        assert_eq!(
            Symbol::from_dex_symbol(Dex::EdgeX, "btcusd"),
            Some(Symbol::Btc)
        );
        assert_eq!(
            Symbol::from_dex_symbol(Dex::Hyperliquid, "hype"),
            Some(Symbol::Hype)
        );
    }

    #[test]
    fn unknown_symbol_is_none() {
        assert_eq!(Symbol::from_dex_symbol(Dex::Hyperliquid, "DOGE"), None);
        // Hyperliquid 表記を edgeX の表として引いても一致しない
        assert_eq!(Symbol::from_dex_symbol(Dex::EdgeX, "BTC"), None);
        assert_eq!(Symbol::from_dex_symbol(Dex::Aster, "BTCUSD"), None);
    }

    #[test]
    fn aster_and_lighter_mappings() {
        assert_eq!(Symbol::Btc.to_dex_symbol(Dex::Aster), "BTCUSDT");
        assert_eq!(Symbol::Hype.to_dex_symbol(Dex::Aster), "HYPEUSDT");
        // Lighter はベースシンボル（market_stats の symbol と照合するためのキー）
        assert_eq!(Symbol::Btc.to_dex_symbol(Dex::Lighter), "BTC");
        assert_eq!(Symbol::Hype.to_dex_symbol(Dex::Lighter), "HYPE");

        // ストリーム名は Aster のみ小文字
        assert_eq!(Symbol::Btc.to_stream_symbol(Dex::Aster), "btcusdt");
        assert_eq!(Symbol::Btc.to_stream_symbol(Dex::Lighter), "BTC");

        // 小文字のストリーム表記からも復元できる
        assert_eq!(
            Symbol::from_dex_symbol(Dex::Aster, "btcusdt"),
            Some(Symbol::Btc)
        );
    }

    #[test]
    fn dex_ordering_is_canonical() {
        // ペア列挙・CSV の dex_a/dex_b の順序はこの並びに依存する
        assert!(Dex::Hyperliquid < Dex::EdgeX);
        assert!(Dex::EdgeX < Dex::Aster);
        assert!(Dex::Aster < Dex::Lighter);
        assert!(Dex::Lighter < Dex::Dydx);
        assert_eq!(Dex::ALL.len(), 5);
    }

    #[test]
    fn dex_parses_from_string() {
        assert_eq!("aster".parse::<Dex>().unwrap(), Dex::Aster);
        assert_eq!("Lighter".parse::<Dex>().unwrap(), Dex::Lighter);
        assert_eq!("dydx".parse::<Dex>().unwrap(), Dex::Dydx);
        assert!("binance".parse::<Dex>().is_err());
    }

    #[test]
    fn dydx_uses_hyphenated_usd_markets() {
        assert_eq!(Symbol::Btc.to_dex_symbol(Dex::Dydx), "BTC-USD");
        assert_eq!(Symbol::Sol.to_dex_symbol(Dex::Dydx), "SOL-USD");
        // 購読メッセージの id にそのまま使うので大文字のまま
        assert_eq!(Symbol::Btc.to_stream_symbol(Dex::Dydx), "BTC-USD");
        assert_eq!(
            Symbol::from_dex_symbol(Dex::Dydx, "eth-usd"),
            Some(Symbol::Eth)
        );
        // 他 DEX の表記とは混同しない
        assert_eq!(Symbol::from_dex_symbol(Dex::Dydx, "BTCUSD"), None);
    }

    #[test]
    fn only_dydx_allows_crossed_books() {
        assert!(Dex::Dydx.allows_crossed_book());
        for dex in [Dex::Hyperliquid, Dex::EdgeX, Dex::Aster, Dex::Lighter] {
            assert!(!dex.allows_crossed_book(), "{dex}");
        }
    }

    #[test]
    fn symbol_serde_uses_canonical_string() {
        let parsed: Vec<Symbol> = serde_json::from_str(r#"["BTC","eth"]"#).unwrap();
        assert_eq!(parsed, vec![Symbol::Btc, Symbol::Eth]);
        assert_eq!(serde_json::to_string(&Symbol::Hype).unwrap(), r#""HYPE""#);
    }
}
