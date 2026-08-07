# dYdX v4 対応 設計指示書

> 全体仕様は `perp-arbitrage-bot-spec.md`、フェーズ1詳細は `phase1-design.md` を参照。本書は3つ目のDEXとしてdYdX v4を追加するための実装指示書。

## 前提

`dex-traits`で定義した`MarketDataSource` traitを実装した`dex-dydx` crateを新規追加する。既存の`market-data` / `recorder` / `bin/collector`には最小限の変更（DEX列挙への追加と設定読み込み）で済むこと。既存2 DEXの実装には影響を与えないこと。

---

## 1. core-typesへの変更

```rust
pub enum Dex {
    Hyperliquid,
    EdgeX,
    Dydx,   // 追加
}
```

`Symbol::to_dex_symbol` / `from_dex_symbol` にdYdXのマッピングを追加する。

dYdXの銘柄表記は `BTC-USD` 形式（ハイフン区切り、USD建て）。

| Symbol | dYdX表記 |
|---|---|
| Btc | `BTC-USD` |
| Eth | `ETH-USD` |
| Sol | `SOL-USD` |
| Hype | 要確認（dYdXにHYPE-USD市場が存在するか実装前に確認すること。存在しない場合はdYdXでは対象外とし、銘柄×DEXの組み合わせが存在しないケースを扱えるようにする） |

> **重要**: 「全銘柄が全DEXに存在する」という前提を置かないこと。`market-data`の乖離計算は、両DEXに板が存在する組み合わせのみを対象とする設計にする。

---

## 2. dex-dydx crate の実装

### 2.1 エンドポイント

dYdX v4はIndexerサービス経由でWebSocketを提供する。

| 環境 | WebSocket URL |
|---|---|
| mainnet | `wss://indexer.dydx.trade/v4/ws` |
| testnet | `wss://indexer.v4testnet.dydx.exchange/v4/ws` |

testnetが公式に提供されているため、フェーズ3の機能検証はこちらを使える。

### 2.2 接続フロー

1. WebSocket接続を確立
2. 接続成功時、サーバーから `connected` タイプの初期メッセージが届く
3. `connected` を受信してから購読メッセージを送る（接続直後に送らないこと）

### 2.3 購読メッセージ

板データは `v4_orderbook` チャンネルを銘柄ごとに購読する。

```json
{ "type": "subscribe", "channel": "v4_orderbook", "id": "BTC-USD" }
```

購読解除:

```json
{ "type": "unsubscribe", "channel": "v4_orderbook", "id": "BTC-USD" }
```

### 2.4 メッセージ処理

- 購読直後に `type: "subscribed"` のメッセージで**全量スナップショット**が届く。このとき既存のローカル板を必ずリセットしてから構築し直すこと
- 以降は `contents` に `bids` / `asks` の差分が届く
- **size が 0 の価格レベルは削除を意味する**。ローカル板から該当価格を除去すること（この処理を誤ると板が永久に残留し、価格差計算が壊れる）
- 各メッセージには `message-id`（論理オフセット）が含まれる。これを順序保証・欠損検知に使う

### 2.5 Ping/Pong（既存2 DEXと挙動が異なるので注意）

dYdX側から30秒ごとにWebSocketの**heartbeat ping制御フレーム**が送られてくる。10秒以内にpongを返さないと切断される。

- これはアプリケーションレベルのJSONメッセージではなく、**WebSocketプロトコルレベルの制御フレーム**である
- `tokio-tungstenite`は多くの場合pongを自動応答するが、実装が自動応答しているか必ず確認すること。していなければ明示的に返す
- edgeXはJSON形式の`{"type":"ping",...}`をアプリ層で送ってくる方式で、dYdXとは仕組みが異なる。両者を同じコードで扱おうとしないこと

---

## 3. dYdX固有の重要な注意点（乖離判定に直結）

### 3.1 板がクロスすることがある

dYdXは中央集権的なオーダーブックを持たないため、**bidがaskより高くなる（クロスした）板が観測されうる**。これは異常データではなく、dYdXの構造上正常に起こる現象。

対応方針:
- `OrderBook`構築時にクロスを検知したらフラグを立て、ログとCSVに記録する
- **クロスしている板から計算した乖離は、アービトラージ機会として扱わない**（フェーズ2の判定ロジックで除外する）
- クロス発生頻度自体をフェーズ1で計測する（dYdXを実運用対象にできるかの判断材料になる）

CSVに以下のカラムを追加:

| カラム | 説明 |
|---|---|
| `book_crossed_a` / `book_crossed_b` | 各DEXの板がクロスしていたか（bool） |

### 3.2 Indexerのデータ鮮度

dYdXのIndexerはブロックチェーンの状態を追随してDBに反映する中間層であり、リアルタイム性はHyperliquidの直接的な板配信より劣る可能性がある。

- 真に正しい板は、その時点のブロックプロポーザーのmempool内にあるものであり、Indexerが見せているものとは差がある
- つまり**dYdXの板は構造的に「少し古い」可能性がある**
- フェーズ1で計測する `staleness_delta_ms` が、dYdXを含む組み合わせで系統的に大きくなっていないかを必ず確認すること。これが大きい場合、dYdXとの乖離の多くは「見かけ上の乖離」である可能性が高い

### 3.3 地理的レイテンシ

HyperliquidはAWS東京リージョンにバリデータが集中しているが、dYdX Indexerのホスティング場所は別である。東京にサーバーを置いた場合、Hyperliquidには極めて近いがdYdXには遠い、という非対称が生じうる。

- フェーズ1でDEXごとのレイテンシ分布を必ず記録し、この非対称の実測値を把握すること

---

## 4. configへの追加

```toml
[dex.dydx]
enabled = true
ws_url = "wss://indexer.dydx.trade/v4/ws"
testnet_ws_url = "wss://indexer.v4testnet.dydx.exchange/v4/ws"
use_testnet = false
reconnect_max_attempts = 10
reconnect_base_delay_ms = 500
# dYdXに存在しない銘柄はここで除外できるようにする
excluded_symbols = []
```

---

## 5. market-data への変更

現状は2 DEX前提の実装になっている可能性があるため、**N個のDEXを扱える構造に一般化**する。

- 3 DEXになると、乖離を計算すべきペアは `Hyperliquid×edgeX`、`Hyperliquid×dYdX`、`edgeX×dYdX` の3通り（一般に N(N-1)/2 通り）
- `DivergenceSnapshot` は既に `dex_a` / `dex_b` を持つ設計なので、ペアを列挙してループする形に変更する
- 有効なDEXの組み合わせは設定から動的に決まるようにし、DEX追加時にコード変更が不要な状態を目指す
- 銘柄がそのDEXに存在しない場合は、そのペアをスキップする

---

## 6. テスト

- dYdXの`subscribed`（全量スナップショット）と差分更新のサンプルJSONを固定データとして持ち、パーサーと板再構築のテストを書く
- **size=0による価格レベル削除**のテストは必ず含めること（ここのバグは板の静かな破損を招き、発見が遅れる）
- クロスした板を入力した場合に、正しくフラグが立ち乖離計算から除外されることのテスト

---

## 7. 実装前に確認すべき事項

- dYdXに `HYPE-USD` 市場が存在するか
- dYdX v4のtaker手数料率（フェーズ2の判定ロジックで必要）
- Indexer WebSocketのレートリミット・同時接続数制限
- 板の深さ（何レベルまで配信されるか）が設定で変えられるか

---

## 8. 段階的な導入手順の推奨

1. まず `dex-dydx` を実装し、**dYdX単体で板が正しく再構築できているか**を確認する（クロス発生率、size=0処理の正しさ）
2. 次に `market-data` をN-DEX対応に一般化する
3. 3 DEX同時稼働でCSVを収集し、`staleness_delta_ms` の分布をDEXペアごとに比較する
4. dYdXを含むペアの乖離が、鮮度差で説明できてしまわないかを検証してから、フェーズ2の判定対象に含めるか判断する
