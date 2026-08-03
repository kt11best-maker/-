use std::time::Duration;

/// 指数バックオフ（上限・ジッタ付き）。
///
/// 全 DEX クライアントで再接続間隔の計算を共通化する。ジッタは複数銘柄・複数 DEX が
/// 同時に切断された際に再接続が同期して殺到するのを防ぐため。
#[derive(Debug, Clone)]
pub struct Backoff {
    base_delay: Duration,
    max_delay: Duration,
    max_attempts: u32,
    attempt: u32,
}

impl Backoff {
    /// `max_attempts = 0` は無制限を意味する。
    pub fn new(base_delay_ms: u64, max_delay_ms: u64, max_attempts: u32) -> Self {
        Backoff {
            base_delay: Duration::from_millis(base_delay_ms.max(1)),
            max_delay: Duration::from_millis(max_delay_ms.max(base_delay_ms.max(1))),
            max_attempts,
            attempt: 0,
        }
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// 接続に成功したら必ず呼ぶ（試行回数をリセットする）。
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    pub fn is_exhausted(&self) -> bool {
        self.max_attempts != 0 && self.attempt >= self.max_attempts
    }

    /// 次の待ち時間を返し、試行回数を 1 進める。上限到達時は `None`。
    pub fn next_delay(&mut self) -> Option<Duration> {
        if self.is_exhausted() {
            return None;
        }
        let delay = self.delay_for(self.attempt);
        self.attempt += 1;
        Some(delay)
    }

    fn delay_for(&self, attempt: u32) -> Duration {
        // base * 2^attempt（オーバーフローさせずに上限で頭打ち）
        let base_ms = self.base_delay.as_millis() as u64;
        let max_ms = self.max_delay.as_millis() as u64;
        let scaled = base_ms.saturating_mul(1u64.checked_shl(attempt.min(32)).unwrap_or(u64::MAX));
        let capped = scaled.min(max_ms);
        Duration::from_millis(apply_jitter(capped))
    }
}

/// ±20% のジッタを乗せる。
///
/// 乱数 crate を足さずに済ませるため、システム時刻のナノ秒成分を種にしている。
/// 再接続タイミングの分散が目的で、暗号強度は不要。
fn apply_jitter(ms: u64) -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    if ms == 0 {
        return 0;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    // 0..=40 の範囲に落として -20%..+20% に写像する
    let pct = (nanos % 41) as i64 - 20;
    let delta = (ms as i64 * pct) / 100;
    (ms as i64 + delta).max(1) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_exponentially_within_jitter_bounds() {
        let mut b = Backoff::new(500, 60_000, 0);
        for expected in [500u64, 1000, 2000, 4000, 8000] {
            let d = b.next_delay().unwrap().as_millis() as u64;
            let lo = expected * 80 / 100;
            let hi = expected * 120 / 100;
            assert!(
                (lo..=hi).contains(&d),
                "attempt delay {d} not near {expected}"
            );
        }
    }

    #[test]
    fn caps_at_max_delay() {
        let mut b = Backoff::new(500, 3_000, 0);
        for _ in 0..20 {
            let d = b.next_delay().unwrap().as_millis() as u64;
            assert!(d <= 3_600, "{d} exceeds max + jitter");
        }
    }

    #[test]
    fn exhausts_after_max_attempts() {
        let mut b = Backoff::new(10, 100, 3);
        assert!(b.next_delay().is_some());
        assert!(b.next_delay().is_some());
        assert!(b.next_delay().is_some());
        assert!(b.is_exhausted());
        assert!(b.next_delay().is_none());
        assert_eq!(b.attempt(), 3);

        b.reset();
        assert!(!b.is_exhausted());
        assert!(b.next_delay().is_some());
    }

    #[test]
    fn zero_max_attempts_is_unlimited() {
        let mut b = Backoff::new(1, 2, 0);
        for _ in 0..1000 {
            assert!(b.next_delay().is_some());
        }
    }
}
