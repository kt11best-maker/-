//! ローカルの WS サーバを立てて、market_stats による market_index の動的解決 →
//! 板購読 → スナップショット → 差分 → nonce 欠損 → 再購読までを通しで検証する。

use std::sync::Arc;
use std::time::Duration;

use config::LighterConfig;
use core_types::{Dex, Symbol};
use dex_lighter::LighterMarketData;
use dex_traits::MarketDataSource;
use futures_util::{SinkExt, StreamExt};
use rust_decimal_macros::dec;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// BTC → market_index 1 のマッピングを配信する。
const MARKET_STATS: &str = r#"{"type":"update/market_stats","channel":"market_stats:all",
    "market_stats":{
      "1":{"symbol":"BTC","market_id":1,"index_price":"36000.0","mark_price":"36000.1",
           "current_funding_rate":"0.0001","funding_rate":"0.0001"},
      "0":{"symbol":"ETH","market_id":0,"index_price":"2000.0","mark_price":"2000.1"}}}"#;

const SNAPSHOT: &str = r#"{"type":"subscribed/order_book","channel":"order_book:1",
    "last_updated_at":1700000000000,"offset":42,
    "order_book":{"code":0,"nonce":100,
      "bids":[{"price":"36000.5","size":"1.5"},{"price":"36000.0","size":"3.0"}],
      "asks":[{"price":"36001.0","size":"2.0"},{"price":"36002.0","size":"1.0"}]}}"#;

const UPDATE_OK: &str = r#"{"type":"update/order_book","channel":"order_book:1",
    "last_updated_at":1700000000050,"offset":999999,
    "order_book":{"begin_nonce":100,"nonce":101,
      "bids":[{"price":"36000.75","size":"0.5"}],"asks":[]}}"#;

const UPDATE_GAP: &str = r#"{"type":"update/order_book","channel":"order_book:1",
    "last_updated_at":1700000000100,
    "order_book":{"begin_nonce":500,"nonce":501,
      "bids":[{"price":"36000.9","size":"0.1"}],"asks":[]}}"#;

fn test_config(addr: std::net::SocketAddr) -> LighterConfig {
    LighterConfig {
        ws_url: format!("ws://{addr}"),
        use_testnet: false,
        keepalive_interval_secs: 60,
        mapping_warn_secs: 300,
        reconnect_max_attempts: 1,
        reconnect_base_delay_ms: 10,
        reconnect_max_delay_ms: 20,
        ..Default::default()
    }
}

#[tokio::test]
async fn resolves_market_index_then_rebuilds_book() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

        // 手順1: まず market_stats:all を購読してくる
        let first = ws.next().await.unwrap().unwrap();
        assert_eq!(
            first.to_text().unwrap(),
            r#"{"type":"subscribe","channel":"market_stats:all"}"#
        );

        // 手順2-3: マッピングを配信すると、対象銘柄の板を購読してくる
        ws.send(Message::text(MARKET_STATS)).await.unwrap();
        let second = ws.next().await.unwrap().unwrap();
        let order_book_sub = second.to_text().unwrap().to_string();

        // 手順4: スナップショット → 差分
        ws.send(Message::text(SNAPSHOT)).await.unwrap();
        ws.send(Message::text(UPDATE_OK)).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        order_book_sub
    });

    let source = LighterMarketData::new(test_config(addr));
    let (tx, mut rx) = mpsc::channel(16);
    let client =
        tokio::spawn(async move { source.subscribe_orderbooks(&[Symbol::Btc], 10, tx).await });

    let snapshot_book = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("スナップショットを受信できなかった")
        .unwrap();
    assert_eq!(snapshot_book.dex, Dex::Lighter);
    assert_eq!(snapshot_book.symbol, Symbol::Btc);
    assert_eq!(snapshot_book.best_bid().unwrap().0, dec!(36000.5));
    assert_eq!(snapshot_book.best_ask().unwrap().0, dec!(36001.0));
    assert_eq!(snapshot_book.trace.exchange_ts_ms, Some(1700000000000));
    snapshot_book.validate().unwrap();

    let updated = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("差分適用後の板を受信できなかった")
        .unwrap();
    assert_eq!(updated.best_bid().unwrap().0, dec!(36000.75));
    assert_eq!(updated.bids.len(), 3);
    updated.validate().unwrap();

    // market_stats の market_id から購読チャンネルを組み立てている
    let order_book_sub = server.await.unwrap();
    assert_eq!(
        order_book_sub,
        r#"{"type":"subscribe","channel":"order_book:1"}"#
    );

    client.abort();
}

#[tokio::test]
async fn resubscribes_when_nonce_gap_is_detected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        ws.next().await.unwrap().unwrap(); // market_stats 購読

        ws.send(Message::text(MARKET_STATS)).await.unwrap();
        ws.next().await.unwrap().unwrap(); // order_book 購読

        ws.send(Message::text(SNAPSHOT)).await.unwrap();
        ws.send(Message::text(UPDATE_GAP)).await.unwrap();

        // 欠損を検知したクライアントは unsubscribe → subscribe を送り直す
        let unsub = ws.next().await.unwrap().unwrap();
        let resub = ws.next().await.unwrap().unwrap();

        // 作り直しの合図として全量スナップショットを返す
        ws.send(Message::text(SNAPSHOT)).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        (
            unsub.to_text().unwrap().to_string(),
            resub.to_text().unwrap().to_string(),
        )
    });

    let source = Arc::new(LighterMarketData::new(test_config(addr)));
    let (tx, mut rx) = mpsc::channel(16);
    let client = {
        let source = Arc::clone(&source);
        tokio::spawn(async move { source.subscribe_orderbooks(&[Symbol::Btc], 10, tx).await })
    };

    let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.best_bid().unwrap().0, dec!(36000.5));

    let (unsub, resub) = server.await.unwrap();
    assert_eq!(unsub, r#"{"type":"unsubscribe","channel":"order_book:1"}"#);
    assert_eq!(resub, r#"{"type":"subscribe","channel":"order_book:1"}"#);

    // 欠損した差分は反映されず、再購読後のスナップショットで作り直される
    let rebuilt = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rebuilt.best_bid().unwrap().0, dec!(36000.5));
    assert!(source.metrics().sequence_gaps > 0);

    client.abort();
}

#[tokio::test]
async fn unresolved_symbols_are_skipped_not_fatal() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        ws.next().await.unwrap().unwrap();

        // BTC のマッピングしか配信しない（HYPE 市場が無いケース）
        ws.send(Message::text(MARKET_STATS)).await.unwrap();
        let sub = ws.next().await.unwrap().unwrap();
        ws.send(Message::text(SNAPSHOT)).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        sub.to_text().unwrap().to_string()
    });

    let source = LighterMarketData::new(test_config(addr));
    let (tx, mut rx) = mpsc::channel(16);
    let client = tokio::spawn(async move {
        source
            .subscribe_orderbooks(&[Symbol::Btc, Symbol::Hype], 10, tx)
            .await
    });

    // 解決できた BTC だけが流れてくる。HYPE が無いことはエラーにならない。
    let book = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("BTC の板を受信できなかった")
        .unwrap();
    assert_eq!(book.symbol, Symbol::Btc);

    let sub = server.await.unwrap();
    assert_eq!(sub, r#"{"type":"subscribe","channel":"order_book:1"}"#);

    client.abort();
}

#[tokio::test]
async fn reports_reconnect_exhausted_when_server_is_unreachable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let source = LighterMarketData::new(test_config(addr));
    let (tx, _rx) = mpsc::channel(4);
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        source.subscribe_orderbooks(&[Symbol::Btc], 10, tx),
    )
    .await
    .expect("再接続上限で終了しなかった");

    assert!(matches!(
        result,
        Err(dex_traits::MarketDataError::ReconnectExhausted { .. })
    ));
}
