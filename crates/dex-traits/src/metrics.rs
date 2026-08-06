use std::sync::atomic::{AtomicU64, Ordering};

/// Market Data ソースの稼働カウンタ（監視タスクが読むスナップショット）。
///
/// DEX ごとに意味のない項目は 0 のままでよい（例: Hyperliquid はフル
/// スナップショット配信なので `sequence_gaps` / `resyncs` は常に 0）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceMetrics {
    /// WS から受け取ったメッセージ数。
    pub messages_received: u64,
    /// 正規化して下流に流した板の数。
    pub books_emitted: u64,
    /// パース失敗・想定外形式の数。
    pub parse_errors: u64,
    /// シーケンス欠損の検知数（差分方式の DEX のみ）。
    pub sequence_gaps: u64,
    /// 板の作り直し（再購読・スナップショット再取得）の回数。
    pub resyncs: u64,
    /// bid > ask（クロスした板）を観測した回数。
    ///
    /// 多くの DEX ではデータ破損のサインだが、**dYdX では構造上正常に起こる**。
    /// dYdX を実運用対象にできるかの判断材料として発生頻度を数える。
    pub crossed_books: u64,
}

/// [`SourceMetrics`] の atomic 版。各 DEX クライアントが内部で持つ。
///
/// WS 受信ループがロック待ちで詰まらないよう、監視側からはロックなしで読める
/// atomic にしている。
#[derive(Debug, Default)]
pub struct SourceCounters {
    messages_received: AtomicU64,
    books_emitted: AtomicU64,
    parse_errors: AtomicU64,
    sequence_gaps: AtomicU64,
    resyncs: AtomicU64,
    crossed_books: AtomicU64,
}

impl SourceCounters {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_message(&self) {
        self.messages_received.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_book(&self) {
        self.books_emitted.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_parse_error(&self) {
        self.parse_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_sequence_gap(&self) {
        self.sequence_gaps.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_resync(&self) {
        self.resyncs.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_crossed_book(&self) {
        self.crossed_books.fetch_add(1, Ordering::Relaxed);
    }

    pub fn crossed_books(&self) -> u64 {
        self.crossed_books.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> SourceMetrics {
        SourceMetrics {
            messages_received: self.messages_received.load(Ordering::Relaxed),
            books_emitted: self.books_emitted.load(Ordering::Relaxed),
            parse_errors: self.parse_errors.load(Ordering::Relaxed),
            sequence_gaps: self.sequence_gaps.load(Ordering::Relaxed),
            resyncs: self.resyncs.load(Ordering::Relaxed),
            crossed_books: self.crossed_books.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate() {
        let c = SourceCounters::new();
        assert_eq!(c.snapshot(), SourceMetrics::default());

        c.record_message();
        c.record_message();
        c.record_book();
        c.record_parse_error();
        c.record_sequence_gap();
        c.record_resync();
        c.record_crossed_book();

        assert_eq!(
            c.snapshot(),
            SourceMetrics {
                messages_received: 2,
                books_emitted: 1,
                parse_errors: 1,
                sequence_gaps: 1,
                resyncs: 1,
                crossed_books: 1,
            }
        );
    }
}
