use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// 対象 DEX。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Dex {
    Hyperliquid,
    EdgeX,
    // 将来: Dydx
}

impl Dex {
    pub const ALL: [Dex; 2] = [Dex::Hyperliquid, Dex::EdgeX];

    pub fn as_str(&self) -> &'static str {
        match self {
            Dex::Hyperliquid => "hyperliquid",
            Dex::EdgeX => "edgex",
        }
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
    pub fn to_dex_symbol(&self, dex: Dex) -> &'static str {
        match dex {
            Dex::Hyperliquid => self.as_str(),
            Dex::EdgeX => match self {
                Symbol::Btc => "BTCUSD",
                Symbol::Eth => "ETHUSD",
                Symbol::Sol => "SOLUSD",
                Symbol::Hype => "HYPEUSD",
            },
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
    }

    #[test]
    fn symbol_serde_uses_canonical_string() {
        let parsed: Vec<Symbol> = serde_json::from_str(r#"["BTC","eth"]"#).unwrap();
        assert_eq!(parsed, vec![Symbol::Btc, Symbol::Eth]);
        assert_eq!(serde_json::to_string(&Symbol::Hype).unwrap(), r#""HYPE""#);
    }
}
