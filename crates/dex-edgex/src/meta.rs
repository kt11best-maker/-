//! contractId の解決。
//!
//! edgeX の depth チャネルは銘柄名ではなく contractId で購読する。ID は
//! 設定ファイルに直書きもできるが、変更に追従できるよう REST のメタデータから
//! 解決するのを既定にしている。

use std::collections::BTreeMap;
use std::time::Duration;

use core_types::{Dex, Symbol};
use dex_traits::MarketDataError;
use serde_json::Value;
use tracing::debug;

/// メタデータ取得のタイムアウト。起動を長く待たせないため短めにする。
const METADATA_TIMEOUT: Duration = Duration::from_secs(10);

/// REST メタデータから「銘柄 → contractId」を解決する。
pub async fn fetch_contract_ids(
    metadata_url: &str,
) -> Result<BTreeMap<Symbol, String>, MarketDataError> {
    let client = reqwest::Client::builder()
        .timeout(METADATA_TIMEOUT)
        .build()
        .map_err(|e| MarketDataError::Config(format!("HTTP クライアントの構築に失敗: {e}")))?;

    let resp = client
        .get(metadata_url)
        .send()
        .await
        .map_err(|e| MarketDataError::Config(format!("メタデータ取得に失敗: {e}")))?;

    if !resp.status().is_success() {
        return Err(MarketDataError::Config(format!(
            "メタデータ取得が HTTP {} を返しました",
            resp.status()
        )));
    }

    let body: Value = resp
        .json()
        .await
        .map_err(|e| MarketDataError::Config(format!("メタデータの JSON 解析に失敗: {e}")))?;

    Ok(extract_contract_ids(&body))
}

/// レスポンス JSON から contractId と contractName の組を拾う。
///
/// レスポンスの入れ子構造（`data.contractList` など）に依存しないよう、
/// 「contractId と contractName を併せ持つオブジェクト」を再帰的に探す。
pub fn extract_contract_ids(body: &Value) -> BTreeMap<Symbol, String> {
    let mut found = BTreeMap::new();
    walk(body, 0, &mut found);
    found
}

fn walk(value: &Value, depth: usize, out: &mut BTreeMap<Symbol, String>) {
    // 想定外に深い JSON でスタックを消費しないための保険
    if depth > 12 {
        return;
    }
    match value {
        Value::Object(map) => {
            if let (Some(id), Some(name)) = (
                map.get("contractId").and_then(as_str_like),
                map.get("contractName").and_then(as_str_like),
            ) {
                if let Some(symbol) = Symbol::from_dex_symbol(Dex::EdgeX, &name) {
                    debug!(symbol = %symbol, contract_id = %id, contract_name = %name, "contractId を解決");
                    out.insert(symbol, id);
                }
            }
            for v in map.values() {
                walk(v, depth + 1, out);
            }
        }
        Value::Array(items) => {
            for v in items {
                walk(v, depth + 1, out);
            }
        }
        _ => {}
    }
}

fn as_str_like(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_from_documented_shape() {
        let body: Value = serde_json::from_str(
            r#"{"code":"SUCCESS","data":{"contractList":[
                {"contractId":"10000001","contractName":"BTCUSD","tickSize":"0.1"},
                {"contractId":"10000002","contractName":"ETHUSD"},
                {"contractId":"10000003","contractName":"SOLUSD"},
                {"contractId":"10000099","contractName":"DOGEUSD"}
            ]}}"#,
        )
        .unwrap();

        let ids = extract_contract_ids(&body);
        assert_eq!(ids.get(&Symbol::Btc).map(String::as_str), Some("10000001"));
        assert_eq!(ids.get(&Symbol::Eth).map(String::as_str), Some("10000002"));
        assert_eq!(ids.get(&Symbol::Sol).map(String::as_str), Some("10000003"));
        // 対象外の銘柄は取り込まない
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn handles_numeric_ids_and_alternative_nesting() {
        let body: Value = serde_json::from_str(
            r#"{"result":{"perpetual":{"list":[{"contractId":10000001,"contractName":"BTCUSD"}]}}}"#,
        )
        .unwrap();
        let ids = extract_contract_ids(&body);
        assert_eq!(ids.get(&Symbol::Btc).map(String::as_str), Some("10000001"));
    }

    #[test]
    fn returns_empty_for_unrelated_json() {
        let body: Value = serde_json::from_str(r#"{"code":"ERROR","msg":"nope"}"#).unwrap();
        assert!(extract_contract_ids(&body).is_empty());
    }
}
