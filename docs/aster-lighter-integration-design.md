# Aster / Lighter 追加 設計指示書（4 DEX構成への拡張）

> 全体仕様は `perp-arbitrage-bot-spec.md`、フェーズ1詳細は `phase1-design.md` を参照。
>
> **重要**: 先に作成した `dydx-integration-design.md` は**破棄**すること。dYdXの導入は取りやめとなった。

## 目的

既存のHyperliquid・edgeXに加え、**Aster**と**Lighter**を追加し、**計4 DEX × 4銘柄（BTC/ETH/SOL/HYPE）**の板データを収集する。この4 DEX間で利益を出せる乖離が実在するかをフェーズ1のデータで検証する。

`dex-traits`で定義した`MarketDataSource` traitを実装した`dex-aster` / `dex-lighter` crateを新規追加する。既存2 DEXの実装には影響を与えないこと。

---

## 1. core-typesへの変更

```rust
pub enum Dex {
    Hyperliquid,
    EdgeX,
    Aster,     // 追加
    Lighter,   // 追加
}
```

### 銘柄マッピング

各DEXで表記が異なるため `Symbol::to_dex_symbol` / `from_dex_symbol` を拡張する。

| Symbol | Aster | Lighter |
|---|---|---|
| Btc | `BTCUSDT`（WSストリーム名では小文字 `btcusdt`） | market_index（起動時にWSで動的取得） |
| Eth | `ETHUSDT` / `ethusdt` | 同上 |
| Sol | `SOLUSDT` / `solusdt` | 同上 |
| Hype | `HYPEUSDT` / `hypeusdt` | 同上 |

**Aster・LighterともにHYPE市場は存在することを確認済み**（4銘柄すべて対象にできる）。

> **重要1**: Lighterは銘柄を**文字列シンボルではなく market_index（数値）** で識別する。起動時に**WebSocketの `market_stats:all` チャンネルを購読**し、返却データに含まれる `symbol` と `market_id` からマッピングを動的に構築してキャッシュすること。ハードコードしないこと。**RESTは使わない**（詳細は3.4節）。
>
> **重要2**: 「全銘柄が全DEXに存在する」前提を置かないこと。HYPEは特に、DEXによっては市場が存在しない可能性が高い。銘柄×DEXの組み合わせが存在しないケースを正常系として扱うこと。

---

## 2. dex-aster crate

### 2.1 エンドポイント

| 用途 | URL |
|---|---|
| WebSocket | `wss://fstream.asterdex.com` |
| REST（板スナップショット） | `https://fapi.asterdex.com/fapi/v1/depth?symbol=BTCUSDT&limit=1000` |
| REST（取引ルール取得） | `https://fapi.asterdex.com/fapi/v1/exchangeInfo` |

> **注意**: Asterには`sapi.asterdex.com`（Spot用）と`fapi.asterdex.com`（Futures/Perp用）がある。本プロジェクトはperpが対象なので**必ずfapi側**を使うこと。

> **注意**: AsterにはPro Mode（CLOB）と1001x/Simple Mode（ALPプールにオラクル価格で約定する方式）がある。**アービトラージ対象はPro ModeのCLOBのみ**。上記fapiエンドポイントがPro Modeの板を返すことを実装時に確認すること。

### 2.2 板の購読と再構築（Binance系の差分更新方式）

ストリーム名: `<symbol>@depth@100ms`（全て小文字）

**板の初期化手順（この順序を厳守すること）**:

1. WebSocketで差分ストリームの購読を開始し、受信イベントをバッファリングする
2. REST `/fapi/v1/depth?symbol=...&limit=1000` でスナップショットを取得し、`lastUpdateId` を得る
3. バッファ内のイベントのうち `u < lastUpdateId` のものは破棄する
4. 最初に処理するイベントは `U <= lastUpdateId AND u >= lastUpdateId` を満たすものであること
5. 以降、各イベントの `pu` が直前イベントの `u` と一致することを検証する。一致しなければ**パケットロスなので手順2からやり直す**

差分イベントのスキーマ:

```json
{
  "e": "depthUpdate",
  "E": 123456789,   // イベント時刻（取引所側タイムスタンプ → MessageTrace.exchange_ts_msに使う）
  "T": 123456788,   // トランザクション時刻
  "s": "BTCUSDT",
  "U": 100,         // このイベント内の最初のupdate ID
  "u": 120,         // このイベント内の最後のupdate ID
  "pu": 99,         // 直前ストリームのu
  "bids": [["0.0024", "10"]],
  "asks": [["0.0026", "100"]]
}
```

**処理上の必須事項**:
- 各イベントの数量は**相対変化ではなく、その価格の絶対数量**。上書きすること
- **数量が0の価格レベルは削除**を意味する
- ローカル板に存在しない価格レベルの削除イベントを受信することがあるが、これは正常。エラー扱いしないこと

### 2.3 接続管理（Aster固有の制約）

- **1本のWS接続は24時間で自動切断される**。これを想定し、切断前に能動的に再接続するか、切断を検知して即座に再接続する仕組みを入れること（フェーズ1は24時間連続稼働が前提なので、ここは必ず踏む）
- サーバーから**5分ごとにping frameが送られる**。15分以内にpongを返さないと切断される
- 未要求のpong frame送信は許可されている（keepaliveに使える）
- **受信メッセージは1秒あたり10件まで**。超過すると切断される
- 1接続あたり最大200ストリームまで購読可能
- レートリミットは**APIキーではなくIP単位**。違反を繰り返すとIP banされ、期間は2分〜3日まで段階的に伸びる

> **設計への反映**: 4銘柄程度なら問題ないが、REST呼び出し（板スナップショット取得、再初期化）が頻発するとIP banのリスクがある。再初期化にはバックオフを入れ、短時間に連続してRESTを叩かないこと。

---

## 3. dex-lighter crate

### 3.1 エンドポイント

| 環境 | WebSocket URL |
|---|---|
| mainnet | `wss://mainnet.zklighter.elliot.ai/stream` |
| testnet | `wss://testnet.zklighter.elliot.ai/stream` |

testnetが提供されているため、フェーズ3の機能検証に使える。

> **方針: Lighterは WebSocket 一本化とする。フェーズ1でRESTは一切使わない。**
> 板データ・シンボルマッピングともにWSで完結できるため、REST側のレートリミットやIP banを考慮する必要がなくなる。

### 3.2 板の購読と再構築

チャンネル名: `order_book:{MARKET_INDEX}`

- 板の更新は**50msごとにバッチで配信**される
- **購読時に完全なスナップショットが届き、以降は差分のみ**が届く
- 差分の連続性は、現在の更新の `begin_nonce` が直前の更新の `nonce`（last_nonce）と一致するかで検証する。一致しなければ再購読して板を作り直す
- `offset` も各更新で増加するが、**連続性は保証されない**（API サーバー側に紐づく値のため）。再接続で別サーバーにルーティングされると大きく変動する。**順序検証には `nonce` を使い、`offset` を使わないこと**

レスポンススキーマ:

```json
{
  "channel": "order_book:{MARKET_INDEX}",
  "last_updated_at": INTEGER,
  "offset": INTEGER,
  "order_book": {
    "code": INTEGER,
    "asks": [{ "price": "...", "size": "..." }],
    "bids": [{ "price": "...", "size": "..." }]
  }
}
```

`last_updated_at` を `MessageTrace.exchange_ts_ms` に使う。

### 3.3 接続管理（Lighter固有の制約）

- **クライアント側が2分に1回以上フレームを送る責任がある**。2分間何も送らないとサーバーが接続を閉じる（Asterと逆で、こちらが能動的にkeepaliveを送る必要がある）
- permessage-deflate圧縮がサポートされている
- **メッセージの読み取りが遅れているクライアントは積極的に切断される**。受信処理でブロックしない設計（受信タスクはchannelに流すだけにする）が必須

### 3.4 シンボル→market_index マッピングの取得（WS経由）

起動時に `market_stats:all` チャンネルを購読する。レスポンスには市場ごとに `symbol`（例: "ETH"）と `market_id`（数値）が含まれるため、ここからマッピングを構築する。

```json
{
  "channel": "market_stats:0",
  "market_stats": {
    "symbol": "ETH",
    "market_id": 0,
    "index_price": "...",
    "mark_price": "...",
    "current_funding_rate": "...",
    "funding_rate": "...",
    ...
  }
}
```

**実装フロー**:
1. WS接続 → `market_stats:all` を購読
2. 受信データから symbol → market_id のマッピングを構築
3. 対象4銘柄（BTC/ETH/SOL/HYPE）の market_index を特定
4. それぞれ `order_book:{MARKET_INDEX}` を購読

`market_stats` は継続的に流れてくるため、マッピングの変化にも自動追随できる。またファンディングレート（`current_funding_rate` / `funding_rate`）もこのチャンネルで取得できるので、将来ファンディング差を判定に組み込む際にも流用できる。

> **手数料率について**: taker/maker手数料は `market_stats` には含まれない。フェーズ2の利益判定を実装する段階で、REST `GET /api/v1/orderBooks` を**起動時に1回だけ**叩いて取得する形にする。フェーズ1では不要なので実装しない。

---

## 4. market-data のN-DEX対応

現状2 DEX前提の実装になっている可能性があるため、**N個のDEXを扱える構造に一般化**する。

- 4 DEXになると乖離を計算すべきペアは **6通り**（一般に N(N-1)/2）:
  Hyperliquid×edgeX、Hyperliquid×Aster、Hyperliquid×Lighter、edgeX×Aster、edgeX×Lighter、Aster×Lighter
- `DivergenceSnapshot` は既に `dex_a` / `dex_b` を持つ設計なので、ペアを列挙してループする形に変更する
- 有効なDEXの組み合わせは設定から動的に決まるようにし、DEX追加時にコード変更が不要な状態を目指す
- **その銘柄が存在しないDEXとのペアはスキップ**する

> **CSVの行数増加に注意**: 6ペア × 4銘柄で、1回の板更新あたり最大24行が生成されうる。フェーズ1の`csv_mode = "all"`だとファイルが急速に肥大化する。最初は数十分だけ動かしてファイルサイズを実測し、24時間分を見積もってから本稼働すること。必要なら`sampled`モードに切り替える。

---

## 5. configへの追加

```toml
[dex.aster]
enabled = true
ws_url = "wss://fstream.asterdex.com"
rest_url = "https://fapi.asterdex.com"
depth_stream_interval = "100ms"      # depth or depth@100ms
snapshot_limit = 1000
reconnect_before_hours = 23          # 24時間切断の前に能動的に再接続
reconnect_max_attempts = 10
reconnect_base_delay_ms = 500
resync_backoff_ms = 5000             # 板再初期化時のバックオフ（IP ban回避）
excluded_symbols = []

[dex.lighter]
enabled = true
ws_url = "wss://mainnet.zklighter.elliot.ai/stream"
testnet_ws_url = "wss://testnet.zklighter.elliot.ai/stream"
use_testnet = false
# フェーズ1ではRESTを使わない（板・シンボルマッピングともWSで完結）
keepalive_interval_secs = 60         # 2分制限に対し余裕を持たせる
reconnect_max_attempts = 10
reconnect_base_delay_ms = 500
excluded_symbols = []
```

---

## 5.5 DEXごとのREST依存の有無（整理）

| DEX | 板の取得方式 | RESTの要否 |
|---|---|---|
| Hyperliquid | 全量スナップショット配信 | 不要の見込み（既存実装を確認すること） |
| edgeX | 要確認 | 差分方式ならスナップショット用にRESTが必要 |
| Aster | 差分更新 | **必須**。初期化時と再同期時に `/fapi/v1/depth` を叩く |
| Lighter | 購読時スナップショット + 差分 | **不要**（WS一本化） |

> **Asterだけは構造的にRESTが外せない**。差分更新方式のため、基準となるスナップショットをRESTでしか取得できず、`pu` の連続性が崩れるたびに再取得が必要になる。ここがIP banのリスク源なので、`resync_backoff_ms` を必ず効かせること。
>
> 既存のHyperliquid / edgeX実装がRESTを使っているかは、実装済みコードを確認して把握しておくこと。使っている場合、そのDEXのレートリミット制約も同様に確認が必要。

---

## 6. DEXごとのkeepalive方式の違い（実装の落とし穴）

4 DEXでping/pongの仕組みがすべて異なる。**共通コードで扱おうとしないこと**。各crateで個別に実装する。

| DEX | 方式 |
|---|---|
| Hyperliquid | 実装済み（既存コードに従う） |
| edgeX | サーバーが**アプリ層のJSON** `{"type":"ping","time":"..."}` を送信 |
| Aster | サーバーが**WSプロトコルレベルのping frame**を5分ごとに送信、15分以内にpong必須 |
| Lighter | **クライアント側が2分に1回以上フレームを送る**責任がある |

---

## 7. テスト

各DEXについて、以下のサンプルJSONを固定データとして持ちパーサー・板再構築のテストを書く:

- Aster: スナップショット + 差分イベント数件。特に **`U`/`u`/`pu` の連続性検証**と**数量0による削除**のテストは必須
- Lighter: 購読時スナップショット + 差分。特に **`begin_nonce`/`nonce` による連続性検証**のテスト
- 存在しない価格レベルの削除イベントを受けても落ちないことのテスト（Asterでは正常系）

> 板の再構築ロジックのバグは「静かに板がずれ続ける」という発見しにくい障害を生む。ここのテストは手を抜かないこと。

---

## 8. 実装前に確認すべき事項

- Asterの `fapi` エンドポイントがPro ModeのCLOB板を返すことの確認
- Lighterの板の深さ（何レベルまで配信されるか）
- 既存のHyperliquid / edgeX実装がRESTに依存しているかの確認
- Aster / Lighter の taker手数料率（**フェーズ2で必要。フェーズ1では不要**）

> HYPE市場はAster・Lighterともに存在することを確認済み。Lighterのmarket_indexは起動時にWSで動的取得するため、事前確認は不要。

---

## 9. 推奨する実装手順

1. `dex-aster` を実装し、**単体で板が正しく再構築できているか**を確認（`pu` 連続性検証、数量0削除、24時間切断への対応）
2. `dex-lighter` を実装し、同様に単体確認（`market_stats:all` によるmarket_index動的取得、`nonce` 連続性検証、クライアント側keepalive）
3. `market-data` をN-DEX対応に一般化する（6ペア列挙）
4. 4 DEX同時稼働で短時間（30分程度）動かし、CSVのサイズと内容を検証
5. 問題なければ東京リージョンのVPSで24時間収集
6. `staleness_delta_ms` の分布をDEXペアごとに比較し、鮮度差で説明できてしまう乖離を除外した上で、真に利益機会がありそうなペアを特定する
