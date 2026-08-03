//! DEX 共通インターフェース。
//!
//! 将来 dYdX を追加する際は、この trait を実装した crate を 1 つ足すだけで
//! 上位レイヤー（collector / market-data）に変更が波及しない設計にしている。

pub mod backoff;
pub mod status;

pub use backoff::Backoff;
pub use status::{ConnectionState, ConnectionStatus};

use async_trait::async_trait;
use core_types::{Dex, OrderBook, Symbol};
use tokio::sync::mpsc;

#[async_trait]
pub trait MarketDataSource: Send + Sync {
    fn dex(&self) -> Dex;

    /// 指定銘柄の板ストリームを開始し、正規化済み [`OrderBook`] を channel に流す。
    ///
    /// 実装は内部で再接続を行い、`reconnect_max_attempts` を使い切った場合にのみ
    /// [`MarketDataError::ReconnectExhausted`] を返して終了する。呼び出し側
    /// （collector の supervisor）はそれを受けてタスクごと再起動する。
    async fn subscribe_orderbooks(
        &self,
        symbols: &[Symbol],
        depth: usize,
        tx: mpsc::Sender<OrderBook>,
    ) -> Result<(), MarketDataError>;

    /// 接続状態（監視・将来のキルスイッチ用）。
    fn connection_status(&self) -> ConnectionStatus;
}

#[derive(Debug, thiserror::Error)]
pub enum MarketDataError {
    #[error("WebSocket エラー: {0}")]
    WebSocket(String),

    #[error("メッセージのパースに失敗: {0}")]
    Parse(String),

    #[error("購読に失敗: {0}")]
    Subscribe(String),

    #[error("設定が不正: {0}")]
    Config(String),

    #[error("下流チャネルが閉じられた")]
    ChannelClosed,

    #[error("再接続の上限に到達 ({attempts} 回)")]
    ReconnectExhausted { attempts: u32 },
}
