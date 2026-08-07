//! ローカルの WS サーバを立てて、connected → 購読 → スナップショット →
//! 差分（size=0 削除を含む）→ クロス板 → message_id 欠損 → 再購読までを
//! 通しで検証する。

use std::sync::Arc;
use std::time::Duration;

use config::DydxConfig;
use core_types::{Dex, Symbol};
use dex_dydx::DydxMarketData;
use dex_traits::MarketDataSource;
use futures_util::{SinkExt, StreamExt};
use rust_decimal_macros::dec;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const CONNECTED: &str = r#"{"type":"connected","connection_id":"conn-1","message_id":0}"#;

const SNAPSHOT: &str = r#"{"type":"subscribed","connection_id":"conn-1","message_id":1,
    "channel":"v4_orderbook","id":"BTC-USD","contents":{
      "bids":[{"price":"36000.5","size":"1.5"},{"price":"36000.0","size":"3.0"}],
      "asks":[{"price":"36001.0","size":"2.0"},{"price":"36002.0","size":"1.0"}]}}"#;

/// 36000.0 を削除し、36000.75 を追加する。
const UPDATE_DELETE: &str = r#"{"type":"channel_data","connection_id":"conn-1","message_id":2,
    "channel":"v4_orderbook","id":"BTC-USD",
    "contents":{"bids":[["36000.75","0.5"],["36000.0","0"]],"asks":[]}}"#;

/// bid が ask を上回る（dYdX では正常に起こる）。
const UPDATE_CROSSED: &str = r#"{"type":"channel_data","connection_id":"conn-1","message_id":3,
    "channel":"v4_orderbook","id":"BTC-USD",
    "contents":{"bids":[["36001.5","1.0"]],"asks":[]}}"#;

/// message_id が飛ぶ。
const UPDATE_GAP: &str = r#"{"type":"channel_data","connection_id":"conn-1","message_id":99,
    "channel":"v4_orderbook","id":"BTC-USD",
    "contents":{"bids":[["36000.9","1.0"]],"asks":[]}}"#;

fn test_config(addr: std::net::SocketAddr) -> DydxConfig {
    DydxConfig {
        ws_url: format!("ws://{addr}"),
        use_testnet: false,
        connected_timeout_secs: 5,
        reconnect_max_attempts: 1,
        reconnect_base_delay_ms: 10,
        reconnect_max_delay_ms: 20,
        ..Default::default()
    }
}

#[tokio::test]
async fn waits_for_connected_then_rebuilds_book() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

        // 接続直後は何も送ってこない（connected を待っている）
        let early = tokio::time::timeout(Duration::from_millis(300), ws.next()).await;
        assert!(early.is_err(), "connected 前に購読メッセージを送っている");

        ws.send(Message::text(CONNECTED)).await.unwrap();
        let sub = ws.next().await.unwrap().unwrap();

        ws.send(Message::text(SNAPSHOT)).await.unwrap();
        ws.send(Message::text(UPDATE_DELETE)).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        sub.to_text().unwrap().to_string()
    });

    let source = DydxMarketData::new(test_config(addr));
    let (tx, mut rx) = mpsc::channel(16);
    let client =
        tokio::spawn(async move { source.subscribe_orderbooks(&[Symbol::Btc], 10, tx).await });

    let snapshot = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("スナップショットを受信できなかった")
        .unwrap();
    assert_eq!(snapshot.dex, Dex::Dydx);
    assert_eq!(snapshot.symbol, Symbol::Btc);
    assert_eq!(snapshot.best_bid().unwrap().0, dec!(36000.5));
    assert_eq!(snapshot.best_ask().unwrap().0, dec!(36001.0));
    assert!(!snapshot.is_crossed());
    snapshot.validate().unwrap();

    let updated = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("差分適用後の板を受信できなかった")
        .unwrap();
    assert_eq!(updated.best_bid().unwrap().0, dec!(36000.75));
    // size 0 の 36000.0 は消えている
    assert_eq!(updated.bids.len(), 2);
    assert!(updated.bids.iter().all(|l| l.price.0 != dec!(36000.0)));

    let sub = server.await.unwrap();
    assert_eq!(
        sub,
        r#"{"type":"subscribe","channel":"v4_orderbook","id":"BTC-USD"}"#
    );

    client.abort();
}

#[tokio::test]
async fn crossed_books_are_delivered_and_counted() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        ws.send(Message::text(CONNECTED)).await.unwrap();
        ws.next().await.unwrap().unwrap();

        ws.send(Message::text(SNAPSHOT)).await.unwrap();
        ws.send(Message::text(UPDATE_DELETE)).await.unwrap();
        ws.send(Message::text(UPDATE_CROSSED)).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
    });

    let source = Arc::new(DydxMarketData::new(test_config(addr)));
    let (tx, mut rx) = mpsc::channel(16);
    let client = {
        let source = Arc::clone(&source);
        tokio::spawn(async move { source.subscribe_orderbooks(&[Symbol::Btc], 10, tx).await })
    };

    let mut crossed = None;
    for _ in 0..3 {
        let book = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("板を受信できなかった")
            .unwrap();
        if book.is_crossed() {
            crossed = Some(book);
        }
    }

    // クロスした板は捨てられずに届く（発生頻度の計測がフェーズ1 の目的）
    let crossed = crossed.expect("クロスした板が届かなかった");
    assert_eq!(crossed.best_bid().unwrap().0, dec!(36001.5));
    assert_eq!(crossed.best_ask().unwrap().0, dec!(36001.0));
    assert!(crossed.validate_allowing_crossed().is_ok());
    assert_eq!(source.metrics().crossed_books, 1);

    server.await.unwrap();
    client.abort();
}

#[tokio::test]
async fn resubscribes_when_message_id_gap_is_detected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        ws.send(Message::text(CONNECTED)).await.unwrap();
        ws.next().await.unwrap().unwrap();

        ws.send(Message::text(SNAPSHOT)).await.unwrap();
        ws.send(Message::text(UPDATE_GAP)).await.unwrap();

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

    let source = Arc::new(DydxMarketData::new(test_config(addr)));
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
    assert_eq!(
        unsub,
        r#"{"type":"unsubscribe","channel":"v4_orderbook","id":"BTC-USD"}"#
    );
    assert_eq!(
        resub,
        r#"{"type":"subscribe","channel":"v4_orderbook","id":"BTC-USD"}"#
    );

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
async fn responds_to_protocol_level_ping() {
    // dYdX は JSON ではなく WS 制御フレームで ping を送ってくる。
    // 10 秒以内に pong を返さないと切断される。
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        ws.send(Message::text(CONNECTED)).await.unwrap();
        ws.next().await.unwrap().unwrap(); // 購読

        ws.send(Message::Ping(vec![1, 2, 3].into())).await.unwrap();
        // pong が返ってくるまで読む（テキストが混ざる可能性に備える）
        loop {
            match ws.next().await.unwrap().unwrap() {
                Message::Pong(payload) => return payload.to_vec(),
                _ => continue,
            }
        }
    });

    let source = DydxMarketData::new(test_config(addr));
    let (tx, _rx) = mpsc::channel(16);
    let client =
        tokio::spawn(async move { source.subscribe_orderbooks(&[Symbol::Btc], 10, tx).await });

    let payload = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("pong が返ってこなかった")
        .unwrap();
    assert_eq!(payload, vec![1, 2, 3]);

    client.abort();
}

#[tokio::test]
async fn reconnects_when_connected_never_arrives() {
    // connected を送ってこないサーバー。購読しないまま張り直しを繰り返す。
    // 接続自体は成功しているので再接続上限には掛からない（一時的に
    // connected が来ないだけで収集を諦めてはいけないため）。
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (accepted_tx, mut accepted_rx) = mpsc::channel(8);

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let accepted_tx = accepted_tx.clone();
            tokio::spawn(async move {
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                let _ = accepted_tx.send(()).await;
                // connected も何も送らない
                while ws.next().await.is_some() {}
            });
        }
    });

    let cfg = DydxConfig {
        connected_timeout_secs: 1,
        ..test_config(addr)
    };
    let source = DydxMarketData::new(cfg);
    let (tx, _rx) = mpsc::channel(4);
    let client =
        tokio::spawn(async move { source.subscribe_orderbooks(&[Symbol::Btc], 10, tx).await });

    // タイムアウトして接続を張り直していること（2 回以上繋ぎに来る）
    for i in 0..2 {
        tokio::time::timeout(Duration::from_secs(5), accepted_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("{}回目の接続が来なかった", i + 1))
            .unwrap();
    }

    client.abort();
}

#[tokio::test]
async fn reports_reconnect_exhausted_when_server_is_unreachable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let source = DydxMarketData::new(test_config(addr));
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
