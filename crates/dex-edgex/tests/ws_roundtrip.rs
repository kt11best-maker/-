//! ローカルの WS サーバを立てて、購読 → スナップショット → 差分 → 欠損検知 →
//! 再購読までを通しで検証する。

use std::collections::BTreeMap;
use std::time::Duration;

use config::EdgeXConfig;
use core_types::{Dex, Symbol};
use dex_edgex::EdgeXMarketData;
use dex_traits::MarketDataSource;
use futures_util::{SinkExt, StreamExt};
use rust_decimal_macros::dec;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const CONTRACT_ID: &str = "10000001";

const SNAPSHOT: &str = r#"{"type":"quote-event","channel":"depth.10000001.15","ts":1700000000000,
    "content":{"dataType":"Snapshot","data":[{"contractId":"10000001",
      "startVersion":"100","endVersion":"100",
      "bids":[["36000.5","1.5"],["36000.0","2.0"]],
      "asks":[["36001.0","2.0"],["36002.0","1.0"]]}]}}"#;

/// 100 → 101 の正常な差分。best bid を 36000.75 に更新する。
const DIFF_OK: &str = r#"{"type":"quote-event","channel":"depth.10000001.15","ts":1700000000100,
    "content":{"dataType":"Changed","data":[{"contractId":"10000001",
      "startVersion":"100","endVersion":"101",
      "bids":[["36000.75","0.5"]],"asks":[]}]}}"#;

/// 101 を飛ばした差分（欠損）。
const DIFF_GAP: &str = r#"{"type":"quote-event","channel":"depth.10000001.15","ts":1700000000200,
    "content":{"dataType":"Changed","data":[{"contractId":"10000001",
      "startVersion":"150","endVersion":"151",
      "bids":[["36000.9","0.1"]],"asks":[]}]}}"#;

fn test_config(addr: std::net::SocketAddr) -> EdgeXConfig {
    let mut contract_ids = BTreeMap::new();
    contract_ids.insert(Symbol::Btc, CONTRACT_ID.to_string());
    EdgeXConfig {
        ws_url: format!("ws://{addr}"),
        // ネットワークを触らせない（テストは contract_ids の手動指定だけで動く）
        resolve_contract_ids: false,
        contract_ids,
        depth_level: 15,
        resync_interval_secs: 0,
        reconnect_max_attempts: 1,
        reconnect_base_delay_ms: 10,
        reconnect_max_delay_ms: 20,
        ..Default::default()
    }
}

#[tokio::test]
async fn rebuilds_book_from_snapshot_and_diff() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

        let subscribe = ws.next().await.unwrap().unwrap();
        assert_eq!(
            subscribe.to_text().unwrap(),
            r#"{"type":"subscribe","channel":"depth.10000001.15"}"#
        );

        ws.send(Message::text(SNAPSHOT)).await.unwrap();
        ws.send(Message::text(DIFF_OK)).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
    });

    let source = EdgeXMarketData::new(test_config(addr));
    let (tx, mut rx) = mpsc::channel(16);
    let client =
        tokio::spawn(async move { source.subscribe_orderbooks(&[Symbol::Btc], 10, tx).await });

    let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("スナップショットを受信できなかった")
        .unwrap();
    assert_eq!(first.dex, Dex::EdgeX);
    assert_eq!(first.symbol, Symbol::Btc);
    assert_eq!(first.best_bid().unwrap().0, dec!(36000.5));
    assert_eq!(first.best_ask().unwrap().0, dec!(36001.0));
    assert_eq!(first.trace.exchange_ts_ms, Some(1700000000000));
    first.validate().unwrap();

    let second = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("差分適用後の板を受信できなかった")
        .unwrap();
    // 差分がローカル板に反映されている
    assert_eq!(second.best_bid().unwrap().0, dec!(36000.75));
    assert_eq!(second.bids.len(), 3);
    second.validate().unwrap();

    server.await.unwrap();
    client.abort();
}

#[tokio::test]
async fn resubscribes_when_sequence_gap_is_detected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        ws.next().await.unwrap().unwrap(); // 最初の subscribe

        ws.send(Message::text(SNAPSHOT)).await.unwrap();
        ws.send(Message::text(DIFF_GAP)).await.unwrap();

        // 欠損を検知したクライアントは unsubscribe → subscribe を送り直す
        let unsub = ws.next().await.unwrap().unwrap();
        let sub = ws.next().await.unwrap().unwrap();

        // 作り直しの合図として全量スナップショットを返す
        ws.send(Message::text(SNAPSHOT)).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        (
            unsub.to_text().unwrap().to_string(),
            sub.to_text().unwrap().to_string(),
        )
    });

    let source = EdgeXMarketData::new(test_config(addr));
    let (tx, mut rx) = mpsc::channel(16);
    let client = tokio::spawn(async move {
        let _ = source.subscribe_orderbooks(&[Symbol::Btc], 10, tx).await;
        source
    });

    // スナップショット分
    let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.best_bid().unwrap().0, dec!(36000.5));

    let (unsub, sub) = server.await.unwrap();
    assert_eq!(
        unsub,
        r#"{"type":"unsubscribe","channel":"depth.10000001.15"}"#
    );
    assert_eq!(sub, r#"{"type":"subscribe","channel":"depth.10000001.15"}"#);

    // 欠損した差分は板に反映されない（再購読後のスナップショットで作り直される）
    let rebuilt = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rebuilt.best_bid().unwrap().0, dec!(36000.5));
    rebuilt.validate().unwrap();

    client.abort();
}

#[tokio::test]
async fn fails_fast_when_no_contract_id_is_available() {
    let cfg = EdgeXConfig {
        ws_url: "ws://127.0.0.1:1".to_string(),
        resolve_contract_ids: false,
        contract_ids: BTreeMap::new(),
        ..Default::default()
    };
    let source = EdgeXMarketData::new(cfg);
    let (tx, _rx) = mpsc::channel(4);
    let result = source.subscribe_orderbooks(&[Symbol::Btc], 10, tx).await;
    assert!(matches!(
        result,
        Err(dex_traits::MarketDataError::Config(_))
    ));
}
