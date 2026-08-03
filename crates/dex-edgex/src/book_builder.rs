//! 差分更新からローカルで板を再構築する。
//!
//! 再構築ロジックのバグは「板が徐々にずれていく」という見つけにくい形で現れるため、
//! 次の 3 段構えで守る:
//!
//! 1. `startVersion` / `endVersion` の連続性チェック（欠損を検知したら再購読）
//! 2. スナップショット受信時に、ローカル板との差異を数えて警告（[`SnapshotCheck`]）
//! 3. 保持レベル数の上限トリム（購読レベル外に取り残された古い価格を溜め込まない）

use std::collections::BTreeMap;

use core_types::{Dex, Level, MessageTrace, OrderBook, Price, Quantity, Symbol};
use rust_decimal::Decimal;

use crate::message::DepthData;

/// スナップショットとローカル板の突き合わせ結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SnapshotCheck {
    /// 適用前にローカル板が構築済みだったか。
    pub was_initialized: bool,
    /// 上位レベルで食い違っていた数（0 なら再構築ロジックは健全）。
    pub mismatched_levels: usize,
}

/// 差分適用の失敗理由。いずれも「再購読してスナップショットを取り直す」が対処。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyError {
    /// スナップショット未受信の状態で差分が届いた。
    NotInitialized,
    /// バージョンが不連続（メッセージ欠損）。
    VersionGap { expected: u64, got: Option<u64> },
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::NotInitialized => write!(f, "スナップショット未受信のまま差分が届いた"),
            ApplyError::VersionGap { expected, got } => {
                write!(f, "バージョン欠損: expected={expected}, got={got:?}")
            }
        }
    }
}

impl std::error::Error for ApplyError {}

/// 突き合わせ時に比較する上位レベル数。
const CHECK_DEPTH: usize = 5;

/// 銘柄 1 つ分のローカル板。
#[derive(Debug)]
pub struct BookBuilder {
    symbol: Symbol,
    /// 価格 → 数量。BTreeMap なので常に価格順に走査できる。
    bids: BTreeMap<Decimal, Decimal>,
    asks: BTreeMap<Decimal, Decimal>,
    last_version: Option<u64>,
    initialized: bool,
    /// 片側あたりの保持上限。購読レベル数に合わせて古い価格を捨てる。
    max_levels: usize,
}

impl BookBuilder {
    pub fn new(symbol: Symbol, max_levels: usize) -> Self {
        BookBuilder {
            symbol,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            last_version: None,
            initialized: false,
            max_levels: max_levels.max(1),
        }
    }

    pub fn symbol(&self) -> Symbol {
        self.symbol
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn last_version(&self) -> Option<u64> {
        self.last_version
    }

    /// 板を破棄して未初期化状態に戻す（再購読時に呼ぶ）。
    pub fn reset(&mut self) {
        self.bids.clear();
        self.asks.clear();
        self.last_version = None;
        self.initialized = false;
    }

    /// 全量スナップショットで板を置き換える。
    ///
    /// 既に板が構築済みの場合は、置き換える前に上位レベルを突き合わせて
    /// 再構築ロジックのズレを検出する。
    pub fn apply_snapshot(&mut self, depth: &DepthData) -> SnapshotCheck {
        let mut bids = BTreeMap::new();
        let mut asks = BTreeMap::new();
        for raw in &depth.bids {
            if let Some((price, size)) = raw.parts() {
                if size > Decimal::ZERO {
                    bids.insert(price, size);
                }
            }
        }
        for raw in &depth.asks {
            if let Some((price, size)) = raw.parts() {
                if size > Decimal::ZERO {
                    asks.insert(price, size);
                }
            }
        }

        let check = SnapshotCheck {
            was_initialized: self.initialized,
            mismatched_levels: if self.initialized {
                count_mismatches(&self.bids, &bids, true)
                    + count_mismatches(&self.asks, &asks, false)
            } else {
                0
            },
        };

        self.bids = bids;
        self.asks = asks;
        self.last_version = depth.end_version_u64();
        self.initialized = true;
        self.trim();
        check
    }

    /// 差分を適用する。数量 0 は「そのレベルを削除」を意味する。
    pub fn apply_diff(&mut self, depth: &DepthData) -> Result<(), ApplyError> {
        if !self.initialized {
            return Err(ApplyError::NotInitialized);
        }
        // バージョンが両方揃っている場合のみ連続性を検査する（片方でも欠ける
        // API 形式なら検査自体をスキップし、スナップショット突き合わせに委ねる）。
        if let (Some(expected), Some(start)) = (self.last_version, depth.start_version_u64()) {
            if start != expected {
                return Err(ApplyError::VersionGap {
                    expected,
                    got: Some(start),
                });
            }
        }

        for raw in &depth.bids {
            if let Some((price, size)) = raw.parts() {
                apply_level(&mut self.bids, price, size);
            }
        }
        for raw in &depth.asks {
            if let Some((price, size)) = raw.parts() {
                apply_level(&mut self.asks, price, size);
            }
        }

        if let Some(end) = depth.end_version_u64() {
            self.last_version = Some(end);
        }
        self.trim();
        Ok(())
    }

    /// 現在の板を正規化済み [`OrderBook`] に変換する。
    ///
    /// `trace` は受信直後に生成したものを渡すこと（この関数で
    /// `normalized_instant` を刻む）。
    pub fn to_order_book(
        &self,
        depth: usize,
        exchange_ts_ms: Option<u64>,
        mut trace: MessageTrace,
    ) -> OrderBook {
        let bids: Vec<Level> = self
            .bids
            .iter()
            .rev()
            .take(depth)
            .map(|(p, q)| Level::new(Price(*p), Quantity(*q)))
            .collect();
        let asks: Vec<Level> = self
            .asks
            .iter()
            .take(depth)
            .map(|(p, q)| Level::new(Price(*p), Quantity(*q)))
            .collect();

        trace.set_exchange_ts_ms(exchange_ts_ms);
        trace.mark_normalized();
        OrderBook::new(Dex::EdgeX, self.symbol, bids, asks, trace)
    }

    /// 保持レベル数を上限に切り詰める。
    ///
    /// 購読ウィンドウの外に出たレベルは以後更新が届かないため、放置すると
    /// 古い価格が残り続け（24 時間稼働ではメモリも板の質も劣化する）。
    fn trim(&mut self) {
        while self.bids.len() > self.max_levels {
            // bid は安い方から捨てる
            let Some(lowest) = self.bids.keys().next().copied() else {
                break;
            };
            self.bids.remove(&lowest);
        }
        while self.asks.len() > self.max_levels {
            // ask は高い方から捨てる
            let Some(highest) = self.asks.keys().next_back().copied() else {
                break;
            };
            self.asks.remove(&highest);
        }
    }
}

fn apply_level(side: &mut BTreeMap<Decimal, Decimal>, price: Decimal, size: Decimal) {
    if size > Decimal::ZERO {
        side.insert(price, size);
    } else {
        side.remove(&price);
    }
}

/// 上位 [`CHECK_DEPTH`] レベルを突き合わせて食い違いを数える。
fn count_mismatches(
    local: &BTreeMap<Decimal, Decimal>,
    snapshot: &BTreeMap<Decimal, Decimal>,
    descending: bool,
) -> usize {
    let collect = |m: &BTreeMap<Decimal, Decimal>| -> Vec<(Decimal, Decimal)> {
        if descending {
            m.iter()
                .rev()
                .take(CHECK_DEPTH)
                .map(|(p, q)| (*p, *q))
                .collect()
        } else {
            m.iter().take(CHECK_DEPTH).map(|(p, q)| (*p, *q)).collect()
        }
    };
    let a = collect(local);
    let b = collect(snapshot);
    let len = a.len().max(b.len());
    (0..len).filter(|i| a.get(*i) != b.get(*i)).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EdgeXEnvelope;
    use rust_decimal_macros::dec;

    fn depth_from(json: &str) -> DepthData {
        let env: EdgeXEnvelope = serde_json::from_str(json).unwrap();
        env.content.unwrap().data.into_iter().next().unwrap()
    }

    fn snapshot_json(start: u64, end: u64) -> String {
        format!(
            r#"{{"type":"quote-event","content":{{"dataType":"Snapshot","data":[{{
                "contractId":"10000001","startVersion":{start},"endVersion":{end},
                "bids":[["100.0","1"],["99.0","2"],["98.0","3"]],
                "asks":[["101.0","1"],["102.0","2"],["103.0","3"]]}}]}}}}"#
        )
    }

    fn builder_with_snapshot() -> BookBuilder {
        let mut b = BookBuilder::new(Symbol::Btc, 20);
        let check = b.apply_snapshot(&depth_from(&snapshot_json(100, 100)));
        assert!(!check.was_initialized);
        b
    }

    fn book_of(b: &BookBuilder) -> OrderBook {
        b.to_order_book(20, Some(1), MessageTrace::on_receive())
    }

    #[test]
    fn snapshot_builds_sorted_book() {
        let b = builder_with_snapshot();
        let book = book_of(&b);
        assert_eq!(book.dex, Dex::EdgeX);
        assert_eq!(book.best_bid(), Some(Price(dec!(100))));
        assert_eq!(book.best_ask(), Some(Price(dec!(101))));
        assert_eq!(book.bids.len(), 3);
        assert_eq!(book.asks.len(), 3);
        book.validate().unwrap();
        assert_eq!(b.last_version(), Some(100));
    }

    #[test]
    fn diff_updates_inserts_and_deletes() {
        let mut b = builder_with_snapshot();
        // 100.5 を追加、99.0 を削除（size=0）、101.0 の数量を変更
        let diff = depth_from(
            r#"{"type":"quote-event","content":{"dataType":"Changed","data":[{
                "contractId":"10000001","startVersion":100,"endVersion":101,
                "bids":[["100.5","0.7"],["99.0","0"]],
                "asks":[["101.0","5"]]}]}}"#,
        );
        b.apply_diff(&diff).unwrap();

        let book = book_of(&b);
        assert_eq!(book.best_bid(), Some(Price(dec!(100.5))));
        assert_eq!(book.bids.len(), 3, "99.0 が消えて 100.5 が増える");
        assert!(book.bids.iter().all(|l| l.price != Price(dec!(99))));
        assert_eq!(book.asks[0].quantity, Quantity(dec!(5)));
        assert_eq!(b.last_version(), Some(101));
        book.validate().unwrap();
    }

    #[test]
    fn sequential_diffs_keep_book_consistent() {
        let mut b = builder_with_snapshot();
        for (start, end, price, size) in [
            (100u64, 101u64, "100.1", "1"),
            (101, 102, "100.2", "2"),
            (102, 103, "100.1", "0"),
        ] {
            let diff = depth_from(&format!(
                r#"{{"type":"quote-event","content":{{"dataType":"Changed","data":[{{
                    "startVersion":{start},"endVersion":{end},
                    "bids":[["{price}","{size}"]],"asks":[]}}]}}}}"#
            ));
            b.apply_diff(&diff).unwrap();
        }
        let book = book_of(&b);
        assert_eq!(book.best_bid(), Some(Price(dec!(100.2))));
        assert!(book.bids.iter().all(|l| l.price != Price(dec!(100.1))));
        assert_eq!(b.last_version(), Some(103));
    }

    #[test]
    fn detects_version_gap() {
        let mut b = builder_with_snapshot();
        let diff = depth_from(
            r#"{"type":"quote-event","content":{"dataType":"Changed","data":[{
                "startVersion":105,"endVersion":106,
                "bids":[["100.5","1"]],"asks":[]}]}}"#,
        );
        assert_eq!(
            b.apply_diff(&diff),
            Err(ApplyError::VersionGap {
                expected: 100,
                got: Some(105)
            })
        );
        // 欠損した差分は板に反映されない
        assert_eq!(book_of(&b).best_bid(), Some(Price(dec!(100))));
    }

    #[test]
    fn rejects_diff_before_snapshot() {
        let mut b = BookBuilder::new(Symbol::Eth, 20);
        let diff = depth_from(
            r#"{"type":"quote-event","content":{"dataType":"Changed","data":[{
                "startVersion":1,"endVersion":2,"bids":[["100","1"]],"asks":[]}]}}"#,
        );
        assert_eq!(b.apply_diff(&diff), Err(ApplyError::NotInitialized));
    }

    #[test]
    fn skips_version_check_when_api_omits_versions() {
        let mut b = builder_with_snapshot();
        let diff = depth_from(
            r#"{"type":"quote-event","content":{"dataType":"Changed","data":[{
                "bids":[["100.5","1"]],"asks":[]}]}}"#,
        );
        assert!(b.apply_diff(&diff).is_ok());
        assert_eq!(book_of(&b).best_bid(), Some(Price(dec!(100.5))));
    }

    #[test]
    fn snapshot_check_reports_no_mismatch_when_in_sync() {
        let mut b = builder_with_snapshot();
        // 同じ内容のスナップショットが再送された場合はズレ 0
        let check = b.apply_snapshot(&depth_from(&snapshot_json(100, 100)));
        assert_eq!(
            check,
            SnapshotCheck {
                was_initialized: true,
                mismatched_levels: 0
            }
        );
    }

    #[test]
    fn snapshot_check_detects_local_drift() {
        let mut b = builder_with_snapshot();
        // 差分を「取りこぼした」状態を作る
        let missed = depth_from(
            r#"{"type":"quote-event","content":{"dataType":"Changed","data":[{
                "startVersion":100,"endVersion":101,
                "bids":[["100.0","0"]],"asks":[]}]}}"#,
        );
        b.apply_diff(&missed).unwrap();
        // 取引所側は 100.0 が生きているスナップショットを返す
        let check = b.apply_snapshot(&depth_from(&snapshot_json(101, 102)));
        assert!(check.was_initialized);
        assert!(check.mismatched_levels > 0, "ズレを検知できていない");
        // 突き合わせ後はスナップショット側が正となる
        assert_eq!(book_of(&b).best_bid(), Some(Price(dec!(100))));
    }

    #[test]
    fn trims_to_max_levels() {
        let mut b = BookBuilder::new(Symbol::Sol, 2);
        b.apply_snapshot(&depth_from(&snapshot_json(1, 1)));
        let book = book_of(&b);
        // 上位 2 レベルだけが残る
        assert_eq!(book.bids.len(), 2);
        assert_eq!(book.asks.len(), 2);
        assert_eq!(book.best_bid(), Some(Price(dec!(100))));
        assert_eq!(book.bids[1].price, Price(dec!(99)));
        assert_eq!(book.asks[1].price, Price(dec!(102)));
    }

    #[test]
    fn trims_after_diffs_so_memory_stays_bounded() {
        let mut b = BookBuilder::new(Symbol::Hype, 3);
        b.apply_snapshot(&depth_from(&snapshot_json(1, 1)));
        for i in 0..100 {
            let price = dec!(90) - Decimal::from(i);
            let diff = depth_from(&format!(
                r#"{{"type":"quote-event","content":{{"dataType":"Changed","data":[{{
                    "bids":[["{price}","1"]],"asks":[]}}]}}}}"#
            ));
            b.apply_diff(&diff).unwrap();
        }
        assert_eq!(b.bids.len(), 3);
        assert_eq!(book_of(&b).best_bid(), Some(Price(dec!(100))));
    }

    #[test]
    fn reset_clears_state() {
        let mut b = builder_with_snapshot();
        b.reset();
        assert!(!b.is_initialized());
        assert_eq!(b.last_version(), None);
        assert!(book_of(&b).best_bid().is_none());
    }

    #[test]
    fn zero_size_levels_in_snapshot_are_ignored() {
        let mut b = BookBuilder::new(Symbol::Btc, 20);
        b.apply_snapshot(&depth_from(
            r#"{"type":"quote-event","content":{"dataType":"Snapshot","data":[{
                "bids":[["100","1"],["99","0"]],"asks":[["101","1"]]}]}}"#,
        ));
        let book = book_of(&b);
        assert_eq!(book.bids.len(), 1);
        book.validate().unwrap();
    }
}
