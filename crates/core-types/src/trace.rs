use std::time::{Duration, Instant};

use crate::now_wall_ms;

/// 1 メッセージのレイテンシ計測情報。
///
/// **重要**: 取引所との時刻比較には必ず wall clock（`*_wall_ms` / `exchange_ts_ms`）を、
/// 自プロセス内の区間計測には必ず [`Instant`]（monotonic）を使う。両者を混同すると、
/// NTP 補正が入った瞬間に区間計測が壊れる。
#[derive(Debug, Clone, Copy)]
pub struct MessageTrace {
    /// 取引所がメッセージ内に埋め込んだ生成時刻（wall clock, ms epoch）。
    /// API が提供しない場合は `None`。
    pub exchange_ts_ms: Option<u64>,
    /// 受信時の `SystemTime`（`exchange_ts_ms` との比較用。NTP 同期前提）。
    pub received_wall_ms: u64,
    /// 受信時の monotonic instant（内部区間計測の起点）。
    pub received_instant: Instant,
    /// パース・正規化完了時点。
    pub normalized_instant: Option<Instant>,
    /// 価格差判定完了時点。
    pub evaluated_instant: Option<Instant>,
}

impl MessageTrace {
    /// WS からメッセージを受け取った**直後・パース前**に呼ぶ。
    pub fn on_receive() -> Self {
        // 順序に意味がある: monotonic を先に取り、パース前の時点を確実に押さえる。
        let received_instant = Instant::now();
        let received_wall_ms = now_wall_ms();
        MessageTrace {
            exchange_ts_ms: None,
            received_wall_ms,
            received_instant,
            normalized_instant: None,
            evaluated_instant: None,
        }
    }

    /// パース中に取引所タイムスタンプが判明した時点で埋める。
    pub fn set_exchange_ts_ms(&mut self, ts: Option<u64>) {
        self.exchange_ts_ms = ts;
    }

    /// 正規化（`OrderBook` 構築）完了時に呼ぶ。
    pub fn mark_normalized(&mut self) {
        self.normalized_instant = Some(Instant::now());
    }

    /// 価格差判定完了時に呼ぶ。
    pub fn mark_evaluated(&mut self) {
        self.evaluated_instant = Some(Instant::now());
    }

    /// 取引所 → 自プロセスの遅延（wall clock 比較, ms）。
    ///
    /// 負値は時計ズレ（NTP 未同期・取引所側の時刻ズレ）の兆候。呼び出し側で
    /// 警告ログを出すこと。
    pub fn exchange_to_local_ms(&self) -> Option<i64> {
        self.exchange_ts_ms
            .map(|ex| self.received_wall_ms as i64 - ex as i64)
    }

    /// 受信 → 正規化完了の処理時間（monotonic）。
    pub fn normalize_latency(&self) -> Option<Duration> {
        self.normalized_instant
            .map(|t| t.saturating_duration_since(self.received_instant))
    }

    /// 受信 → 判定完了の処理時間（monotonic）。
    pub fn total_pipeline_latency(&self) -> Option<Duration> {
        self.evaluated_instant
            .map(|t| t.saturating_duration_since(self.received_instant))
    }

    /// 板データの鮮度（現在時刻 - 受信時刻, ms）。monotonic で測る。
    pub fn age(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.received_instant)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exchange_to_local_handles_clock_skew() {
        let mut t = MessageTrace::on_receive();
        t.received_wall_ms = 1_000_000;
        t.exchange_ts_ms = Some(999_950);
        assert_eq!(t.exchange_to_local_ms(), Some(50));

        // 取引所時刻が未来 = 時計ズレ。負値がそのまま返り、呼び出し側が検知できる。
        t.exchange_ts_ms = Some(1_000_120);
        assert_eq!(t.exchange_to_local_ms(), Some(-120));

        t.exchange_ts_ms = None;
        assert_eq!(t.exchange_to_local_ms(), None);
    }

    #[test]
    fn latencies_are_none_until_marked() {
        let mut t = MessageTrace::on_receive();
        assert!(t.normalize_latency().is_none());
        assert!(t.total_pipeline_latency().is_none());

        t.mark_normalized();
        assert!(t.normalize_latency().is_some());
        assert!(t.total_pipeline_latency().is_none());

        t.mark_evaluated();
        let total = t.total_pipeline_latency().unwrap();
        assert!(total >= t.normalize_latency().unwrap());
    }
}
