//! 板スナップショットの REST 取得。
//!
//! Aster は差分更新方式のため、基準となるスナップショットは REST でしか取れない。
//! ここが唯一の REST 依存であり、IP ban のリスク源でもある。呼び出し側は必ず
//! `resync_backoff_ms` で叩く間隔を空けること。

use std::time::Duration;

use dex_traits::MarketDataError;

use crate::message::DepthSnapshot;

/// スナップショット取得のタイムアウト。
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);

/// REST クライアントを作る（接続プールを使い回すため 1 つを共有すること）。
pub fn build_client() -> Result<reqwest::Client, MarketDataError> {
    reqwest::Client::builder()
        .timeout(SNAPSHOT_TIMEOUT)
        .build()
        .map_err(|e| MarketDataError::Config(format!("HTTP クライアントの構築に失敗: {e}")))
}

/// `/fapi/v1/depth` から板スナップショットを取得する。
pub async fn fetch_depth_snapshot(
    client: &reqwest::Client,
    url: &str,
) -> Result<DepthSnapshot, MarketDataError> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| MarketDataError::WebSocket(format!("スナップショット取得に失敗: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        // 418/429 はレートリミット。バックオフが効いているか確認できるよう
        // ステータスをそのまま残す。
        return Err(MarketDataError::WebSocket(format!(
            "スナップショット取得が HTTP {status} を返しました"
        )));
    }

    resp.json::<DepthSnapshot>()
        .await
        .map_err(|e| MarketDataError::Parse(format!("スナップショットの JSON 解析に失敗: {e}")))
}
