use std::collections::HashMap;
use std::time::{Duration, Instant};

/// キーごとに「最後に通した時刻」を覚えて、警告ログの流量を絞る。
///
/// レイテンシ異常や鮮度差の警告は、異常時に毎メッセージ発火しうる。24 時間
/// 稼働させる前提ではログファイルが肥大化するため、必ずここを通す。
pub struct RateLimiter {
    min_interval: Duration,
    last: HashMap<String, Instant>,
    /// 抑制した回数（次に通したときに件数として報告する）。
    suppressed: HashMap<String, u64>,
}

impl RateLimiter {
    pub fn new(min_interval: Duration) -> Self {
        RateLimiter {
            min_interval,
            last: HashMap::new(),
            suppressed: HashMap::new(),
        }
    }

    /// 通してよければ `Some(抑制されていた件数)` を返す。
    pub fn allow(&mut self, key: &str) -> Option<u64> {
        self.allow_at(key, Instant::now())
    }

    fn allow_at(&mut self, key: &str, now: Instant) -> Option<u64> {
        match self.last.get(key) {
            Some(prev) if now.duration_since(*prev) < self.min_interval => {
                *self.suppressed.entry(key.to_string()).or_insert(0) += 1;
                None
            }
            _ => {
                self.last.insert(key.to_string(), now);
                Some(self.suppressed.remove(key).unwrap_or(0))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suppresses_within_interval_and_reports_count() {
        let mut rl = RateLimiter::new(Duration::from_secs(10));
        let t0 = Instant::now();

        assert_eq!(rl.allow_at("a", t0), Some(0));
        assert_eq!(rl.allow_at("a", t0 + Duration::from_secs(1)), None);
        assert_eq!(rl.allow_at("a", t0 + Duration::from_secs(2)), None);
        // 間隔を空ければ通り、その間に抑制した件数が分かる
        assert_eq!(rl.allow_at("a", t0 + Duration::from_secs(11)), Some(2));
        assert_eq!(rl.allow_at("a", t0 + Duration::from_secs(30)), Some(0));
    }

    #[test]
    fn keys_are_independent() {
        let mut rl = RateLimiter::new(Duration::from_secs(10));
        let t0 = Instant::now();
        assert_eq!(rl.allow_at("a", t0), Some(0));
        assert_eq!(rl.allow_at("b", t0), Some(0));
        assert_eq!(rl.allow_at("a", t0), None);
    }
}
