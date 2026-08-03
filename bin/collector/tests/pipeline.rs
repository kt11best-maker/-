//! 両 DEX の生 JSON から CSV 1 行までを通しで検証する統合テスト。
//!
//! ネットワークには一切触れず、実 API のレスポンス形式に合わせた固定サンプルを
//! パーサに食わせて、価格差計算 → CSV 出力までの結線を確認する。

use std::sync::Arc;

use config::{CsvMode, RecordingConfig};
use core_types::{Dex, MessageTrace, OrderBook, Symbol};
use dex_edgex::{BookBuilder, EdgeXEnvelope};
use dex_hyperliquid::{to_order_book, HlMessage};
use market_data::BookStore;
use recorder::{CsvRecorder, CSV_HEADER};
use rust_decimal_macros::dec;

/// Hyperliquid: mid = 36000.75
const HL_L2_BOOK: &str = r#"{"channel":"l2Book","data":{"coin":"BTC","time":1700000000000,
    "levels":[[{"px":"36000.5","sz":"5","n":3},{"px":"36000.0","sz":"10","n":4}],
              [{"px":"36001.0","sz":"5","n":2},{"px":"36002.0","sz":"10","n":3}]]}}"#;

/// edgeX: mid = 36010.75（Hyperliquid より約 2.8bps 高い）
const EDGEX_SNAPSHOT: &str = r#"{"type":"quote-event","channel":"depth.10000001.15","ts":1700000000010,
    "content":{"dataType":"Snapshot","data":[{"contractId":"10000001",
      "startVersion":"100","endVersion":"100",
      "bids":[["36010.5","5"],["36010.0","10"]],
      "asks":[["36011.0","5"],["36012.0","10"]]}]}}"#;

fn hyperliquid_book(depth: usize) -> OrderBook {
    let HlMessage::L2Book { data } = serde_json::from_str(HL_L2_BOOK).unwrap() else {
        panic!("l2Book として解釈されなかった");
    };
    let mut trace = MessageTrace::on_receive();
    trace.received_wall_ms = 1_700_000_000_050;
    to_order_book(&data, depth, trace).unwrap()
}

fn edgex_book(depth: usize) -> OrderBook {
    let env: EdgeXEnvelope = serde_json::from_str(EDGEX_SNAPSHOT).unwrap();
    let mut builder = BookBuilder::new(Symbol::Btc, 15);
    builder.apply_snapshot(env.depth().unwrap());
    let mut trace = MessageTrace::on_receive();
    trace.received_wall_ms = 1_700_000_000_070;
    builder.to_order_book(depth, env.exchange_ts_ms(), trace)
}

#[test]
fn raw_messages_flow_through_to_a_csv_row() {
    let hl = hyperliquid_book(20);
    let edgex = edgex_book(20);
    hl.validate().unwrap();
    edgex.validate().unwrap();

    let store = Arc::new(BookStore::new());
    store.update(hl);
    store.update(edgex);

    // 集約タスクと同じ向き（A=Hyperliquid, B=edgeX）で計算する
    let snapshot = store
        .divergence(
            Symbol::Btc,
            Dex::Hyperliquid,
            Dex::EdgeX,
            Dex::EdgeX,
            Some(dec!(36000)),
        )
        .expect("価格差を計算できなかった");

    assert_eq!(snapshot.mid_a.0, dec!(36000.75));
    assert_eq!(snapshot.mid_b.0, dec!(36010.75));
    // edgeX の方が高い → raw は負
    assert!(snapshot.raw_spread_bps < dec!(0));
    // 取れる方向は「edgeX で売り、Hyperliquid で買い」
    assert_eq!(
        snapshot.executable_direction,
        market_data::ExecutableDirection::SellBBuyA
    );
    // 板の鮮度差（Hyperliquid 受信 50ms - edgeX 受信 70ms）
    assert_eq!(snapshot.staleness_delta_ms, -20);
    // 取引所 → 受信の遅延
    assert_eq!(snapshot.exchange_latency_ms(Dex::Hyperliquid), Some(50));
    assert_eq!(snapshot.exchange_latency_ms(Dex::EdgeX), Some(60));
    assert!(snapshot.vwap_spread_bps.is_some());

    let dir = tempfile::tempdir().unwrap();
    let cfg = RecordingConfig {
        csv_dir: dir.path().to_path_buf(),
        csv_mode: CsvMode::All,
        ..Default::default()
    };
    {
        let mut recorder = CsvRecorder::new(&cfg);
        recorder.record(&snapshot).unwrap();
        recorder.flush().unwrap();
        assert_eq!(recorder.rows_written(), 1);
    }

    let path = dir.path().join(format!(
        "{}_BTC.csv",
        chrono::DateTime::from_timestamp_millis(snapshot.computed_at_wall_ms as i64)
            .unwrap()
            .format("%Y-%m-%d")
    ));
    let content = std::fs::read_to_string(&path).unwrap();
    let mut lines = content.lines();

    let header: Vec<&str> = lines.next().unwrap().split(',').collect();
    assert_eq!(header, CSV_HEADER.to_vec());

    let row: Vec<&str> = lines.next().unwrap().split(',').collect();
    assert_eq!(row.len(), CSV_HEADER.len());
    let col = |name: &str| row[header.iter().position(|h| *h == name).unwrap()];

    assert_eq!(col("symbol"), "BTC");
    assert_eq!(col("dex_a"), "hyperliquid");
    assert_eq!(col("dex_b"), "edgex");
    assert_eq!(col("mid_a"), "36000.75");
    assert_eq!(col("mid_b"), "36010.75");
    assert_eq!(col("best_bid_a"), "36000.5");
    assert_eq!(col("best_ask_b"), "36011");
    assert_eq!(col("staleness_delta_ms"), "-20");
    assert_eq!(col("latency_a_ms"), "50");
    assert_eq!(col("latency_b_ms"), "60");
    // 10bps 深さ: 36000.5 の ±10bps は約 ±36 なので両レベルが入る
    assert_eq!(col("depth_a_bps10"), "15");
    assert_eq!(col("depth_b_bps10"), "15");
    assert!(col("raw_spread_bps").starts_with('-'));
    assert!(!col("vwap_spread_bps").is_empty());
    assert!(!col("pipeline_latency_us").is_empty());
    assert!(lines.next().is_none(), "余分な行が書かれている");
}

#[test]
fn one_sided_market_produces_no_row() {
    let store = BookStore::new();
    store.update(hyperliquid_book(20));
    // edgeX 側が未受信なら価格差は計算しない（片側だけの見かけの値を残さない）
    assert!(store
        .divergence(
            Symbol::Btc,
            Dex::Hyperliquid,
            Dex::EdgeX,
            Dex::Hyperliquid,
            None
        )
        .is_none());
}
