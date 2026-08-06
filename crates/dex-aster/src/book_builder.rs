//! REST スナップショット + WS 差分からローカルで板を再構築する。
//!
//! # 初期化手順（設計書の順序を厳守）
//!
//! 1. WS の差分購読を開始し、受信イベントをバッファリングする
//! 2. REST でスナップショットを取得し `lastUpdateId` を得る
//! 3. バッファ内の `u < lastUpdateId` のイベントは破棄する
//! 4. 最初に処理するイベントは `U <= lastUpdateId AND u >= lastUpdateId` を満たすこと
//! 5. 以降、各イベントの `pu` が直前イベントの `u` と一致することを検証する。
//!    一致しなければパケットロスなので手順 2 からやり直す
//!
//! この順序を崩すと板が静かにずれ続けるため、[`BookPhase`] で状態を明示的に
//! 持ち、逸脱をすべてエラーとして表面化させている。

use std::collections::{BTreeMap, VecDeque};

use core_types::{Dex, Level, MessageTrace, OrderBook, Price, Quantity, Symbol};
use rust_decimal::Decimal;

use crate::message::{DepthEvent, DepthSnapshot};

/// 板の再構築フェーズ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookPhase {
    /// スナップショット待ち。届く差分はバッファに溜める。
    AwaitingSnapshot,
    /// スナップショット適用済み。最初の差分イベントを待っている。
    AwaitingFirstEvent { last_update_id: u64 },
    /// 差分の連続適用中。
    Streaming { last_update_id: u64 },
}

/// 差分適用の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// スナップショット未取得のためバッファに退避した。
    Buffered,
    /// 既に反映済みの古いイベントなので捨てた。
    Discarded,
    /// 板に反映した。
    Applied,
}

/// 差分適用の失敗。いずれも「スナップショットを取り直す」が対処。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyError {
    /// スナップショットが古すぎて最初のイベントと繋がらない
    /// （`U > lastUpdateId`）。
    SnapshotTooOld {
        last_update_id: u64,
        first_update_id: u64,
    },
    /// `pu` が直前の `u` と一致しない（パケットロス）。
    SequenceGap { expected: u64, got: u64 },
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::SnapshotTooOld {
                last_update_id,
                first_update_id,
            } => write!(
                f,
                "スナップショットが古い: lastUpdateId={last_update_id}, U={first_update_id}"
            ),
            ApplyError::SequenceGap { expected, got } => {
                write!(f, "シーケンス欠損: pu={got}, 期待値={expected}")
            }
        }
    }
}

impl std::error::Error for ApplyError {}

/// バッファに溜める差分イベントの上限。
///
/// REST スナップショットの取得が遅れても青天井にメモリを食わないよう、古い
/// ものから捨てる。捨てた場合は連続性検証で必ず検知され、再取得に落ちる。
const MAX_BUFFERED_EVENTS: usize = 2_000;

/// 銘柄 1 つ分のローカル板。
#[derive(Debug)]
pub struct AsterBookBuilder {
    symbol: Symbol,
    bids: BTreeMap<Decimal, Decimal>,
    asks: BTreeMap<Decimal, Decimal>,
    phase: BookPhase,
    buffer: VecDeque<DepthEvent>,
    max_levels: usize,
    /// 取引所側タイムスタンプ（最後に反映したイベント/スナップショットのもの）。
    exchange_ts_ms: Option<u64>,
}

impl AsterBookBuilder {
    pub fn new(symbol: Symbol, max_levels: usize) -> Self {
        AsterBookBuilder {
            symbol,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            phase: BookPhase::AwaitingSnapshot,
            buffer: VecDeque::new(),
            max_levels: max_levels.max(1),
            exchange_ts_ms: None,
        }
    }

    pub fn symbol(&self) -> Symbol {
        self.symbol
    }

    pub fn phase(&self) -> BookPhase {
        self.phase
    }

    /// 板として公開できる状態か（スナップショット適用済みか）。
    pub fn has_book(&self) -> bool {
        !matches!(self.phase, BookPhase::AwaitingSnapshot)
    }

    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }

    /// 板を破棄してスナップショット待ちに戻す。
    pub fn reset(&mut self) {
        self.bids.clear();
        self.asks.clear();
        self.buffer.clear();
        self.phase = BookPhase::AwaitingSnapshot;
        self.exchange_ts_ms = None;
    }

    /// REST スナップショットを適用し、バッファ済みイベントを流し込む。
    ///
    /// バッファ適用中に連続性が崩れた場合はエラーを返す。呼び出し側は
    /// バックオフを挟んでスナップショットを取り直すこと。
    pub fn apply_snapshot(&mut self, snapshot: &DepthSnapshot) -> Result<(), ApplyError> {
        self.bids = to_side(&snapshot.bids);
        self.asks = to_side(&snapshot.asks);
        self.phase = BookPhase::AwaitingFirstEvent {
            last_update_id: snapshot.last_update_id,
        };
        self.exchange_ts_ms = snapshot.event_time_ms;
        self.trim();

        // バッファを取り出してから流し込む（apply_event が再度バッファへ積むのを防ぐ）
        let buffered: Vec<DepthEvent> = self.buffer.drain(..).collect();
        for event in buffered {
            self.apply_event(&event)?;
        }
        Ok(())
    }

    /// 差分イベントを適用する。
    pub fn apply_event(&mut self, event: &DepthEvent) -> Result<ApplyOutcome, ApplyError> {
        match self.phase {
            BookPhase::AwaitingSnapshot => {
                if self.buffer.len() >= MAX_BUFFERED_EVENTS {
                    self.buffer.pop_front();
                }
                self.buffer.push_back(event.clone());
                Ok(ApplyOutcome::Buffered)
            }
            BookPhase::AwaitingFirstEvent { last_update_id } => {
                // 手順3: スナップショットより古いイベントは捨てる
                if event.final_update_id < last_update_id {
                    return Ok(ApplyOutcome::Discarded);
                }
                // 手順4: U <= lastUpdateId AND u >= lastUpdateId
                if event.first_update_id > last_update_id {
                    return Err(ApplyError::SnapshotTooOld {
                        last_update_id,
                        first_update_id: event.first_update_id,
                    });
                }
                self.apply_levels(event);
                self.phase = BookPhase::Streaming {
                    last_update_id: event.final_update_id,
                };
                Ok(ApplyOutcome::Applied)
            }
            BookPhase::Streaming { last_update_id } => {
                if event.final_update_id <= last_update_id {
                    // 再送・重複。板は既に新しいので無視してよい。
                    return Ok(ApplyOutcome::Discarded);
                }
                // 手順5: pu が直前の u と一致すること。
                // pu を提供しない実装もあるため、無い場合は検証をスキップする。
                if let Some(pu) = event.prev_final_update_id {
                    if pu != last_update_id {
                        return Err(ApplyError::SequenceGap {
                            expected: last_update_id,
                            got: pu,
                        });
                    }
                }
                self.apply_levels(event);
                self.phase = BookPhase::Streaming {
                    last_update_id: event.final_update_id,
                };
                Ok(ApplyOutcome::Applied)
            }
        }
    }

    fn apply_levels(&mut self, event: &DepthEvent) {
        for raw in &event.bids {
            if let Some((price, qty)) = raw.parts() {
                upsert(&mut self.bids, price, qty);
            }
        }
        for raw in &event.asks {
            if let Some((price, qty)) = raw.parts() {
                upsert(&mut self.asks, price, qty);
            }
        }
        if let Some(ts) = event.event_time_ms {
            self.exchange_ts_ms = Some(ts);
        }
        self.trim();
    }

    /// 現在の板を正規化済み [`OrderBook`] に変換する。
    pub fn to_order_book(&self, depth: usize, mut trace: MessageTrace) -> OrderBook {
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

        trace.set_exchange_ts_ms(self.exchange_ts_ms);
        trace.mark_normalized();
        OrderBook::new(Dex::Aster, self.symbol, bids, asks, trace)
    }

    /// 保持レベル数を上限に切り詰める。
    ///
    /// 差分ストリームは板の全域を配信するため、上限を設けないと遠い価格の
    /// レベルが際限なく溜まる（24 時間稼働ではメモリを食い続ける）。
    fn trim(&mut self) {
        while self.bids.len() > self.max_levels {
            let Some(lowest) = self.bids.keys().next().copied() else {
                break;
            };
            self.bids.remove(&lowest);
        }
        while self.asks.len() > self.max_levels {
            let Some(highest) = self.asks.keys().next_back().copied() else {
                break;
            };
            self.asks.remove(&highest);
        }
    }
}

fn to_side(levels: &[crate::message::RawLevel]) -> BTreeMap<Decimal, Decimal> {
    let mut side = BTreeMap::new();
    for raw in levels {
        if let Some((price, qty)) = raw.parts() {
            if qty > Decimal::ZERO {
                side.insert(price, qty);
            }
        }
    }
    side
}

/// 数量は絶対値なので上書き。0 は削除。
///
/// ローカル板に無い価格の削除イベントが届くことがあるが、これは正常
/// （`BTreeMap::remove` は存在しなければ何もしない）。
fn upsert(side: &mut BTreeMap<Decimal, Decimal>, price: Decimal, qty: Decimal) {
    if qty > Decimal::ZERO {
        side.insert(price, qty);
    } else {
        side.remove(&price);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::parse_depth_message;
    use rust_decimal_macros::dec;

    const SNAPSHOT_JSON: &str = r#"{
        "lastUpdateId": 120,
        "E": 1700000000100,
        "bids": [["36000.5","1.5"],["36000.0","2.0"],["35999.0","3.0"]],
        "asks": [["36001.0","2.0"],["36002.0","1.0"],["36003.0","4.0"]]
    }"#;

    fn snapshot() -> DepthSnapshot {
        serde_json::from_str(SNAPSHOT_JSON).unwrap()
    }

    fn event(u_first: u64, u_last: u64, pu: Option<u64>, bids: &str, asks: &str) -> DepthEvent {
        let pu_field = match pu {
            Some(v) => format!(r#""pu":{v},"#),
            None => String::new(),
        };
        let raw = format!(
            r#"{{"e":"depthUpdate","E":1700000000200,"s":"BTCUSDT",
                "U":{u_first},"u":{u_last},{pu_field}"b":[{bids}],"a":[{asks}]}}"#
        );
        parse_depth_message(&raw).unwrap().unwrap()
    }

    fn builder() -> AsterBookBuilder {
        AsterBookBuilder::new(Symbol::Btc, 100)
    }

    fn book(b: &AsterBookBuilder) -> OrderBook {
        b.to_order_book(20, MessageTrace::on_receive())
    }

    #[test]
    fn snapshot_builds_sorted_book() {
        let mut b = builder();
        b.apply_snapshot(&snapshot()).unwrap();

        let book = book(&b);
        assert_eq!(book.dex, Dex::Aster);
        assert_eq!(book.symbol, Symbol::Btc);
        assert_eq!(book.best_bid(), Some(Price(dec!(36000.5))));
        assert_eq!(book.best_ask(), Some(Price(dec!(36001.0))));
        assert_eq!(book.trace.exchange_ts_ms, Some(1700000000100));
        assert!(b.has_book());
        assert_eq!(
            b.phase(),
            BookPhase::AwaitingFirstEvent {
                last_update_id: 120
            }
        );
        book.validate().unwrap();
    }

    #[test]
    fn first_event_must_straddle_last_update_id() {
        let mut b = builder();
        b.apply_snapshot(&snapshot()).unwrap();

        // u < lastUpdateId は破棄
        assert_eq!(
            b.apply_event(&event(100, 119, Some(99), "", "")).unwrap(),
            ApplyOutcome::Discarded
        );
        // U <= 120 <= u なら採用
        assert_eq!(
            b.apply_event(&event(118, 125, Some(117), r#"["36000.75","0.5"]"#, ""))
                .unwrap(),
            ApplyOutcome::Applied
        );
        assert_eq!(
            b.phase(),
            BookPhase::Streaming {
                last_update_id: 125
            }
        );
        assert_eq!(book(&b).best_bid(), Some(Price(dec!(36000.75))));
    }

    #[test]
    fn detects_snapshot_too_old() {
        let mut b = builder();
        b.apply_snapshot(&snapshot()).unwrap();
        // U > lastUpdateId → スナップショットが古い
        assert_eq!(
            b.apply_event(&event(200, 210, Some(199), "", "")),
            Err(ApplyError::SnapshotTooOld {
                last_update_id: 120,
                first_update_id: 200,
            })
        );
    }

    #[test]
    fn validates_pu_continuity() {
        let mut b = builder();
        b.apply_snapshot(&snapshot()).unwrap();
        b.apply_event(&event(118, 125, Some(117), "", "")).unwrap();

        // pu == 直前の u なら適用
        assert_eq!(
            b.apply_event(&event(126, 130, Some(125), r#"["36000.6","1"]"#, ""))
                .unwrap(),
            ApplyOutcome::Applied
        );
        // pu が飛んでいたら欠損
        assert_eq!(
            b.apply_event(&event(140, 150, Some(139), "", "")),
            Err(ApplyError::SequenceGap {
                expected: 130,
                got: 139,
            })
        );
        // 欠損したイベントは板に反映されていない
        assert_eq!(book(&b).best_bid(), Some(Price(dec!(36000.6))));
    }

    #[test]
    fn zero_quantity_deletes_level() {
        let mut b = builder();
        b.apply_snapshot(&snapshot()).unwrap();
        b.apply_event(&event(118, 125, Some(117), r#"["36000.5","0"]"#, ""))
            .unwrap();

        let book = book(&b);
        assert_eq!(book.best_bid(), Some(Price(dec!(36000.0))));
        assert!(book.bids.iter().all(|l| l.price != Price(dec!(36000.5))));
    }

    #[test]
    fn deleting_unknown_level_is_normal() {
        let mut b = builder();
        b.apply_snapshot(&snapshot()).unwrap();
        // ローカル板に存在しない価格の削除イベント（Aster では正常系）
        let outcome = b
            .apply_event(&event(
                118,
                125,
                Some(117),
                r#"["1.0","0"]"#,
                r#"["999999.0","0"]"#,
            ))
            .unwrap();
        assert_eq!(outcome, ApplyOutcome::Applied);
        let book = book(&b);
        assert_eq!(book.bids.len(), 3);
        assert_eq!(book.asks.len(), 3);
        book.validate().unwrap();
    }

    #[test]
    fn events_are_buffered_until_snapshot_arrives() {
        let mut b = builder();
        // スナップショット前に届いた差分はバッファされる
        assert_eq!(
            b.apply_event(&event(100, 110, Some(99), r#"["35000.0","1"]"#, ""))
                .unwrap(),
            ApplyOutcome::Buffered
        );
        assert_eq!(
            b.apply_event(&event(111, 125, Some(110), r#"["36000.75","0.5"]"#, ""))
                .unwrap(),
            ApplyOutcome::Buffered
        );
        assert_eq!(b.buffered_len(), 2);
        assert!(!b.has_book());

        // スナップショット適用時にバッファが流し込まれる。
        // u=110 のイベントは lastUpdateId=120 より古いので破棄され、
        // U=111 <= 120 <= u=125 のイベントだけが反映される。
        b.apply_snapshot(&snapshot()).unwrap();
        assert_eq!(b.buffered_len(), 0);
        assert_eq!(
            b.phase(),
            BookPhase::Streaming {
                last_update_id: 125
            }
        );

        let book = book(&b);
        assert_eq!(book.best_bid(), Some(Price(dec!(36000.75))));
        // バッファ済みの古いイベント（35000.0）は反映されない
        assert!(book.bids.iter().all(|l| l.price != Price(dec!(35000.0))));
    }

    #[test]
    fn buffered_gap_surfaces_on_snapshot_apply() {
        let mut b = builder();
        b.apply_event(&event(118, 125, Some(117), "", "")).unwrap();
        // pu が繋がらないイベントをバッファに入れる
        b.apply_event(&event(140, 150, Some(139), "", "")).unwrap();

        assert_eq!(
            b.apply_snapshot(&snapshot()),
            Err(ApplyError::SequenceGap {
                expected: 125,
                got: 139,
            })
        );
    }

    #[test]
    fn buffer_is_bounded() {
        let mut b = builder();
        for i in 0..(MAX_BUFFERED_EVENTS + 100) as u64 {
            b.apply_event(&event(i, i + 1, Some(i), "", "")).unwrap();
        }
        assert_eq!(b.buffered_len(), MAX_BUFFERED_EVENTS);
    }

    #[test]
    fn duplicate_events_are_discarded() {
        let mut b = builder();
        b.apply_snapshot(&snapshot()).unwrap();
        b.apply_event(&event(118, 125, Some(117), "", "")).unwrap();
        // 既に反映済みの範囲は捨てる（pu 検証も走らせない）
        assert_eq!(
            b.apply_event(&event(118, 125, Some(117), "", "")).unwrap(),
            ApplyOutcome::Discarded
        );
    }

    #[test]
    fn tolerates_missing_pu() {
        let mut b = builder();
        b.apply_snapshot(&snapshot()).unwrap();
        b.apply_event(&event(118, 125, None, "", "")).unwrap();
        assert_eq!(
            b.apply_event(&event(126, 130, None, r#"["36000.6","1"]"#, ""))
                .unwrap(),
            ApplyOutcome::Applied
        );
    }

    #[test]
    fn reset_returns_to_awaiting_snapshot() {
        let mut b = builder();
        b.apply_snapshot(&snapshot()).unwrap();
        b.reset();
        assert_eq!(b.phase(), BookPhase::AwaitingSnapshot);
        assert!(!b.has_book());
        assert!(book(&b).best_bid().is_none());
    }

    #[test]
    fn trims_to_max_levels() {
        let mut b = AsterBookBuilder::new(Symbol::Btc, 2);
        b.apply_snapshot(&snapshot()).unwrap();
        let book = book(&b);
        assert_eq!(book.bids.len(), 2);
        assert_eq!(book.asks.len(), 2);
        assert_eq!(book.best_bid(), Some(Price(dec!(36000.5))));
        assert_eq!(book.asks[1].price, Price(dec!(36002.0)));
    }

    #[test]
    fn sequential_diffs_keep_book_consistent() {
        let mut b = builder();
        b.apply_snapshot(&snapshot()).unwrap();

        let steps = [
            (118u64, 125u64, 117u64, r#"["36000.6","1"]"#),
            (126, 130, 125, r#"["36000.7","2"]"#),
            (131, 140, 130, r#"["36000.6","0"]"#),
        ];
        for (u_first, u_last, pu, bids) in steps {
            assert_eq!(
                b.apply_event(&event(u_first, u_last, Some(pu), bids, ""))
                    .unwrap(),
                ApplyOutcome::Applied
            );
        }

        let book = book(&b);
        assert_eq!(book.best_bid(), Some(Price(dec!(36000.7))));
        assert!(book.bids.iter().all(|l| l.price != Price(dec!(36000.6))));
        assert_eq!(
            b.phase(),
            BookPhase::Streaming {
                last_update_id: 140
            }
        );
        book.validate().unwrap();
    }
}
