//! ローカルの WS サーバを立てて、購読 → 受信 → 正規化 → channel 送出までを通しで検証する。

use std::time::Duration;

use config::HyperliquidConfig;
use core_types::{Dex, Symbol};
use dex_hyperliquid::HyperliquidMarketData;
use dex_traits::MarketDataSource;
use futures_util::{SinkExt, StreamExt};
use rust_decimal_macros::dec;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const L2_BOOK: &str = r#"{"channel":"l2Book","data":{"coin":"BTC","time":1700000000000,
    "levels":[[{"px":"36000.5","sz":"1.5","n":3},{"px":"36000.0","sz":"2.0","n":1}],
              [{"px":"36001.0","sz":"2.0","n":2},{"px":"36002.0","sz":"1.0","n":1}]]}}"#;

fn test_config(addr: std::net::SocketAddr) -> HyperliquidConfig {
    HyperliquidConfig {
        ws_url: format!("ws://{addr}"),
        // 再接続で挙動が紛れないよう 1 回で諦めさせる
        reconnect_max_attempts: 1,
        reconnect_base_delay_ms: 10,
        reconnect_max_delay_ms: 20,
        ..Default::default()
    }
}

#[tokio::test]
async fn subscribes_and_emits_normalized_book() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

        // 購読メッセージが API 仕様どおりに届く
        let subscribe = ws.next().await.unwrap().unwrap();
        let text = subscribe.to_text().unwrap().to_string();
        assert!(text.contains(r#""type":"l2Book""#), "{text}");
        assert!(text.contains(r#""coin":"BTC""#), "{text}");

        ws.send(Message::text(L2_BOOK)).await.unwrap();
        // クライアント側が受信しきるまで接続を維持する
        tokio::time::sleep(Duration::from_secs(2)).await;
        text
    });

    let source = HyperliquidMarketData::new(test_config(addr));
    let (tx, mut rx) = mpsc::channel(16);
    let client = tokio::spawn(async move {
        let _ = source.subscribe_orderbooks(&[Symbol::Btc], 10, tx).await;
        source
    });

    let book = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("板を受信できなかった")
        .expect("channel が閉じた");

    assert_eq!(book.dex, Dex::Hyperliquid);
    assert_eq!(book.symbol, Symbol::Btc);
    assert_eq!(book.best_bid().unwrap().0, dec!(36000.5));
    assert_eq!(book.best_ask().unwrap().0, dec!(36001.0));
    assert_eq!(book.trace.exchange_ts_ms, Some(1700000000000));
    // 受信 → 正規化の区間が monotonic で計測されている
    assert!(book.trace.normalize_latency().is_some());
    book.validate().unwrap();

    server.await.unwrap();
    client.abort();
}

#[tokio::test]
async fn survives_disconnect_and_resubscribes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        // 1 回目: 板を 1 件送って即切断する
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        ws.next().await.unwrap().unwrap();
        ws.send(Message::text(L2_BOOK)).await.unwrap();
        ws.close(None).await.unwrap();
        drop(ws);

        // 2 回目: 再接続してきたら購読し直していることを確認して再度送る
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let resubscribe = ws.next().await.unwrap().unwrap();
        assert!(resubscribe.to_text().unwrap().contains(r#""coin":"BTC""#));
        ws.send(Message::text(L2_BOOK)).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
    });

    let source = HyperliquidMarketData::new(test_config(addr));
    let (tx, mut rx) = mpsc::channel(16);
    let client =
        tokio::spawn(async move { source.subscribe_orderbooks(&[Symbol::Btc], 10, tx).await });

    // 切断を挟んで 2 件受信できる = 自動再接続が機能している
    for _ in 0..2 {
        let book = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("再接続後の板を受信できなかった")
            .expect("channel が閉じた");
        assert_eq!(book.symbol, Symbol::Btc);
    }

    server.await.unwrap();
    client.abort();
}

#[tokio::test]
async fn reports_reconnect_exhausted_when_server_is_unreachable() {
    // 誰も listen していないポートに向ける
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let source = HyperliquidMarketData::new(test_config(addr));
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
    assert_eq!(
        source.connection_status(),
        dex_traits::ConnectionStatus::Disconnected
    );
}
