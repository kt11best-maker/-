//! ローカルの WS + HTTP サーバを立てて、購読 → バッファ → REST スナップショット →
//! 差分適用 → 欠損検知 → 再同期までを通しで検証する。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use config::AsterConfig;
use core_types::{Dex, Symbol};
use dex_aster::AsterMarketData;
use dex_traits::MarketDataSource;
use futures_util::SinkExt;
use rust_decimal_macros::dec;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const SNAPSHOT_BODY: &str = r#"{"lastUpdateId":120,"E":1700000000100,
    "bids":[["36000.5","1.5"],["36000.0","2.0"]],
    "asks":[["36001.0","2.0"],["36002.0","1.0"]]}"#;

/// `U=118 <= 120 <= u=125` を満たす、スナップショットに繋がる差分。
const DIFF_OK: &str = r#"{"stream":"btcusdt@depth@100ms","data":{
    "e":"depthUpdate","E":1700000000200,"s":"BTCUSDT","U":118,"u":125,"pu":117,
    "b":[["36000.75","0.5"]],"a":[]}}"#;

/// `pu` が繋がらない差分（パケットロス相当）。
const DIFF_GAP: &str = r#"{"stream":"btcusdt@depth@100ms","data":{
    "e":"depthUpdate","E":1700000000300,"s":"BTCUSDT","U":200,"u":210,"pu":199,
    "b":[["36000.9","0.1"]],"a":[]}}"#;

/// 板スナップショットを返す最小の HTTP サーバ。リクエスト数を数える。
async fn spawn_snapshot_server(requests: Arc<AtomicUsize>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            requests.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                // リクエストヘッダは読み捨てる
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    SNAPSHOT_BODY.len(),
                    SNAPSHOT_BODY
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });

    addr
}

fn test_config(ws: SocketAddr, rest: SocketAddr) -> AsterConfig {
    AsterConfig {
        ws_url: format!("ws://{ws}"),
        rest_url: format!("http://{rest}"),
        snapshot_limit: 100,
        // テストを速く回すため短くする（本番は IP ban 回避のため秒オーダー）
        resync_backoff_ms: 50,
        reconnect_before_hours: 0,
        reconnect_max_attempts: 1,
        reconnect_base_delay_ms: 10,
        reconnect_max_delay_ms: 20,
        ..Default::default()
    }
}

#[tokio::test]
async fn rebuilds_book_from_snapshot_and_diff() {
    let requests = Arc::new(AtomicUsize::new(0));
    let rest_addr = spawn_snapshot_server(Arc::clone(&requests)).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        // 差分を流し続ける（スナップショット到着前の分はクライアントがバッファする）
        for _ in 0..20 {
            if ws.send(Message::text(DIFF_OK)).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    });

    let source = AsterMarketData::new(test_config(ws_addr, rest_addr)).unwrap();
    let (tx, mut rx) = mpsc::channel(64);
    let client =
        tokio::spawn(async move { source.subscribe_orderbooks(&[Symbol::Btc], 10, tx).await });

    // スナップショットと差分の到着順はタイミング次第なので、差分が反映された
    // 板が出てくるまで読む。
    let mut saw_diff = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(book)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await else {
            break;
        };
        assert_eq!(book.dex, Dex::Aster);
        assert_eq!(book.symbol, Symbol::Btc);
        assert_eq!(book.best_ask().unwrap().0, dec!(36001.0));
        book.validate().unwrap();

        if book.best_bid().unwrap().0 == dec!(36000.75) {
            saw_diff = true;
            break;
        }
    }
    assert!(saw_diff, "差分が板に反映されなかった");
    assert!(
        requests.load(Ordering::SeqCst) >= 1,
        "REST が呼ばれていない"
    );

    server.abort();
    client.abort();
}

#[tokio::test]
async fn resyncs_via_rest_when_sequence_gap_is_detected() {
    let requests = Arc::new(AtomicUsize::new(0));
    let rest_addr = spawn_snapshot_server(Arc::clone(&requests)).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        // 正常な差分をしばらく流してからシーケンスを飛ばす
        for _ in 0..6 {
            let _ = ws.send(Message::text(DIFF_OK)).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        for _ in 0..10 {
            let _ = ws.send(Message::text(DIFF_GAP)).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    });

    let source = Arc::new(AsterMarketData::new(test_config(ws_addr, rest_addr)).unwrap());
    let (tx, mut rx) = mpsc::channel(64);
    let client = {
        let source = Arc::clone(&source);
        tokio::spawn(async move { source.subscribe_orderbooks(&[Symbol::Btc], 10, tx).await })
    };

    // 板は流れ続ける（欠損しても再同期して復帰する）
    let first = tokio::time::timeout(Duration::from_secs(8), rx.recv())
        .await
        .expect("板を受信できなかった")
        .unwrap();
    first.validate().unwrap();

    // 欠損検知 → REST 再取得が走るまで待つ
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        if source.metrics().sequence_gaps > 0 && requests.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let metrics = source.metrics();
    assert!(
        metrics.sequence_gaps > 0,
        "シーケンス欠損を検知できていない"
    );
    assert!(
        requests.load(Ordering::SeqCst) >= 2,
        "再同期の REST 呼び出しが行われていない: {}",
        requests.load(Ordering::SeqCst)
    );
    assert!(metrics.books_emitted > 0);

    server.abort();
    client.abort();
}

#[tokio::test]
async fn reports_reconnect_exhausted_when_server_is_unreachable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_addr = listener.local_addr().unwrap();
    drop(listener);
    let rest_addr = ws_addr;

    let source = AsterMarketData::new(test_config(ws_addr, rest_addr)).unwrap();
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

#[tokio::test]
async fn rejects_empty_symbol_list() {
    let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let source = AsterMarketData::new(test_config(addr, addr)).unwrap();
    let (tx, _rx) = mpsc::channel(4);
    let result = source.subscribe_orderbooks(&[], 10, tx).await;
    assert!(matches!(
        result,
        Err(dex_traits::MarketDataError::Config(_))
    ));
}
