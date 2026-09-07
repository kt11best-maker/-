//! ファンディングレート取得の共通インターフェース。
//!
//! **[`MarketDataSource`](crate::MarketDataSource) とは別 trait にしてある。**
//! DEX によっては WS で提供されず REST ポーリングになる可能性があり、そもそも
//! 取得できない DEX もあるため、板の購読と同じ trait に混ぜると
//! 「板は取れるがファンディングは取れない DEX」を表現できなくなる。
//!
//! # 板の収集を阻害しないこと（最優先の制約）
//!
//! フェーズ1 の主目的は板データの収集で、ファンディングは「将来の検討用に
//! 取っておく」おまけである。したがって:
//!
//! - ファンディングの送信は [`FundingChannel::publish`] 経由の**ノンブロッキング**。
//!   下流が詰まっていれば**捨てる**（件数だけ数える）。板の受信ループを
//!   1 ms でも止めないため。
//! - ファンディングのパース失敗は板の処理に影響させない。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use core_types::{Dex, FundingRate, Symbol};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::MarketDataError;

#[async_trait]
pub trait FundingRateSource: Send + Sync {
    fn dex(&self) -> Dex;

    /// ファンディングレートのストリームを開始する。
    ///
    /// 実装は「板と同じ接続に相乗りする」形でも構わない。その場合はこのメソッドで
    /// 送信先を登録し（[`FundingChannel::serve`]）、受信側が閉じるまで待てばよい。
    /// 対応していない DEX はこの trait を実装しない。
    async fn subscribe_funding(
        &self,
        symbols: &[Symbol],
        tx: mpsc::Sender<FundingRate>,
    ) -> Result<(), MarketDataError>;
}

/// ファンディングレートの送信先スロット。
///
/// 板の購読と同じ WS 接続からファンディングを取り出す DEX（Lighter /
/// Hyperliquid）のために、**接続を増やさず**に送信先を後から差し込めるように
/// している。未登録なら publish は黙って捨てられる。
#[derive(Debug, Default)]
pub struct FundingChannel {
    tx: Mutex<Option<mpsc::Sender<FundingRate>>>,
    published: AtomicU64,
    dropped: AtomicU64,
}

impl FundingChannel {
    pub fn new() -> Self {
        Self::default()
    }

    /// 送信先を登録する。
    pub fn register(&self, tx: mpsc::Sender<FundingRate>) {
        *self.lock() = Some(tx);
    }

    pub fn clear(&self) {
        *self.lock() = None;
    }

    pub fn is_registered(&self) -> bool {
        self.lock().is_some()
    }

    pub fn published(&self) -> u64 {
        self.published.load(Ordering::Relaxed)
    }

    /// 下流が詰まっていたために捨てたレート数。
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// レートを下流に流す。**決してブロックしない。**
    ///
    /// 下流が詰まっていれば捨てて `false` を返す。ファンディングは更新頻度が
    /// 低く、1 件落ちても次の更新で回復するため、板の受信を待たせるより捨てる方が
    /// 望ましい。
    pub fn publish(&self, rate: FundingRate) -> bool {
        let Some(tx) = self.lock().clone() else {
            return false;
        };
        match tx.try_send(rate) {
            Ok(()) => {
                self.published.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(mpsc::error::TrySendError::Full(rate)) => {
                let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                // 1 件目と 100 件ごとだけ出す（板のログを埋めないため）
                if dropped == 1 || dropped % 100 == 0 {
                    warn!(
                        dex = %rate.dex,
                        symbol = %rate.symbol,
                        dropped_total = dropped,
                        "ファンディングチャネルが詰まったため破棄"
                    );
                }
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                debug!("ファンディングチャネルが閉じています。登録を解除します");
                self.clear();
                false
            }
        }
    }

    /// 送信先を登録し、受信側が閉じるまで待つ。
    ///
    /// [`FundingRateSource::subscribe_funding`] の実装本体として使う。板と同じ
    /// 接続を使うため、ここで新しい接続は張らない。
    pub async fn serve(
        &self,
        dex: Dex,
        tx: mpsc::Sender<FundingRate>,
    ) -> Result<(), MarketDataError> {
        info!(dex = %dex, "ファンディングレートの収集を開始（板と同じ接続に相乗り）");
        self.register(tx.clone());
        // 受信側が全て drop されるまで待つ。板側のセッションが再接続しても
        // 登録は維持されるので、ここでやることは無い。
        tx.closed().await;
        self.clear();
        info!(
            dex = %dex,
            published = self.published(),
            dropped = self.dropped(),
            "ファンディングレートの収集を終了"
        );
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<mpsc::Sender<FundingRate>>> {
        // 送信中に panic しても収集を止めたくないので、毒された lock も使い続ける
        self.tx.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::MessageTrace;
    use rust_decimal_macros::dec;

    fn rate(dex: Dex) -> FundingRate {
        FundingRate {
            dex,
            symbol: Symbol::Btc,
            current_rate: dec!(0.0001),
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
    fn publish_without_registration_is_a_no_op() {
        let ch = FundingChannel::new();
        assert!(!ch.is_registered());
        assert!(!ch.publish(rate(Dex::Lighter)));
        assert_eq!(ch.published(), 0);
        assert_eq!(ch.dropped(), 0, "未登録は「詰まり」ではない");
    }

    #[tokio::test]
    async fn publish_delivers_to_registered_receiver() {
        let ch = FundingChannel::new();
        let (tx, mut rx) = mpsc::channel(4);
        ch.register(tx);

        assert!(ch.publish(rate(Dex::Hyperliquid)));
        assert_eq!(ch.published(), 1);
        assert_eq!(rx.recv().await.unwrap().dex, Dex::Hyperliquid);
    }

    #[tokio::test]
    async fn full_channel_drops_instead_of_blocking() {
        let ch = FundingChannel::new();
        let (tx, _rx) = mpsc::channel(1);
        ch.register(tx);

        assert!(ch.publish(rate(Dex::Lighter)));
        // 2 件目は入らない。ここでブロックしたら板の受信が止まる
        assert!(!ch.publish(rate(Dex::Lighter)));
        assert_eq!(ch.published(), 1);
        assert_eq!(ch.dropped(), 1);
    }

    #[tokio::test]
    async fn closed_channel_unregisters_itself() {
        let ch = FundingChannel::new();
        let (tx, rx) = mpsc::channel(4);
        ch.register(tx);
        drop(rx);

        assert!(!ch.publish(rate(Dex::Lighter)));
        assert!(!ch.is_registered());
    }

    #[tokio::test]
    async fn serve_returns_when_receiver_is_dropped() {
        let ch = FundingChannel::new();
        let (tx, rx) = mpsc::channel::<FundingRate>(4);
        let served = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(ch.serve(Dex::Lighter, tx), async {
                tokio::task::yield_now().await;
                drop(rx);
            })
        })
        .await
        .expect("serve が終わらない");
        served.0.unwrap();
        assert!(!ch.is_registered());
    }
}
