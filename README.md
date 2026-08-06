# perp-arb-bot — フェーズ1（Market Data 収集基盤）

DEX パーペチュアル・アービトラージ Bot のフェーズ1 実装。
**実弾は一切扱わない。** 4 DEX（Hyperliquid / edgeX / Aster / Lighter）× 4 銘柄
（BTC/ETH/SOL/HYPE）の板を購読し、価格乖離とレイテンシを 24 時間収集して
CSV / JSON ログに残すところまでが範囲。

発注・署名・状態機械・キルスイッチ・証拠金管理はフェーズ2 以降で、このリポジトリには
まだ存在しない。

## クイックスタート

```bash
cargo test --workspace          # ユニット + 統合テスト（ネットワーク不要）
cargo run --release -p collector -- --config config/collector.toml
```

停止は `Ctrl-C`（SIGINT）または `SIGTERM`。CSV を flush してから終了する。

出力先:

- `data/YYYY-MM-DD_<SYMBOL>.csv` — 価格差スナップショット（銘柄ごと・日次）
- `logs/collector.log.YYYY-MM-DD` — 運用イベント（JSON, 日次ローテーション）

ログレベルは `RUST_LOG=debug` で上書きできる（設定ファイルより優先）。

## crate 構成

```
perp-arb-bot/
├── crates/
│   ├── core-types/       # Price / Quantity / OrderBook / MessageTrace など共通型
│   ├── dex-traits/       # MarketDataSource trait・接続状態・バックオフ
│   ├── dex-hyperliquid/  # Hyperliquid の Market Data（WS + パース）
│   ├── dex-edgex/        # edgeX の Market Data（WS + 差分再構築 + contractId 解決）
│   ├── dex-aster/        # Aster の Market Data（WS 差分 + REST スナップショット）
│   ├── dex-lighter/      # Lighter の Market Data（WS 一本化・market_index 動的解決）
│   ├── market-data/      # BookStore・ペア列挙・価格差計算
│   ├── recorder/         # JSON ログ初期化・CSV 出力
│   └── config/           # TOML 設定
└── bin/collector/        # 実行バイナリ（タスク結線・supervisor・監視）
```

依存方向は一方向（`bin → crates`、`dex-* → dex-traits → core-types`）。DEX を追加する
場合は `MarketDataSource` を実装した crate を 1 つ足し、`config` にセクションを、
`bin/collector` の `build_sources` に 1 分岐を足すだけでよい。ペア列挙・集約・記録・
監視はいずれも trait 越しに扱うため変更不要。

## タスク構成

```
起動
 ├─ config 読み込み → ログ初期化 → BookStore 初期化
 ├─ spawn: 各 DEX の WS 購読タスク（有効なものだけ）─→ mpsc<OrderBook>
 ├─ spawn: 集約タスク（ペアごとに価格差を計算）    ─→ mpsc<DivergenceSnapshot>
 ├─ spawn: CSV writer タスク（バッファリングして定期 flush）
 ├─ spawn: 監視タスク（接続状態・レイテンシ・スループット）
 └─ SIGINT/SIGTERM → CSV を flush して正常終了
```

- CSV 書き込みは WS 受信タスクと分離し、ディスク I/O が受信をブロックしない。
  集約 → CSV の送信はノンブロッキングで、詰まった場合は破棄件数を記録する
  （`snapshots_dropped`）。
- 各 Market Data タスクは supervisor 配下で動き、異常終了・panic しても他の DEX を
  巻き込まずに再起動する。
- 板の保持は「(DEX × 銘柄) の最新のみ」、差分方式 DEX のローカル板は保持レベル数を
  トリム、警告ログはレート制限付き。24 時間稼働でメモリ・ファイルが際限なく伸びない。

### 比較ペアの列挙

有効な DEX が N 個なら比較ペアは **N(N-1)/2 通り**（4 DEX なら 6 ペア）。ペアの
向きは `Dex` の宣言順（hyperliquid → edgex → aster → lighter）に正規化されるため、
CSV の `dex_a`/`dex_b` の並びと符号の意味は行ごとに変わらない。

板の更新はどれか 1 つの DEX でしか起きないので、**更新された DEX を含むペアだけ**を
再計算する（4 DEX なら 1 更新あたり 3 行）。その銘柄の板がまだ揃っていないペア
（= その DEX に市場が無い、未受信）は**正常系として黙ってスキップ**する。
「全銘柄が全 DEX に存在する」前提は置かない。

> **CSV の行数に注意**: 6 ペア × 4 銘柄では `csv_mode = "all"` のファイル増加が
> 速い。まず数十分だけ動かして実測し、24 時間分を見積もってから本稼働すること。
> 必要なら `sampled` に切り替える（設定のみで、コード変更は不要）。

## 24 時間稼働の運用メモ

- ログは日次ローテーション。CSV は銘柄ごと・日次に分割される。
- 初期は `csv_mode = "all"`（全件記録）。ファイルサイズを見てから `sampled` /
  `threshold` に切り替える。切替は設定ファイルのみで、コード変更は不要。
- `logs/` の JSON を `jq` で追うと、接続断・再接続・シーケンス欠損・レイテンシ異常が
  すべて構造化フィールドで取れる。

```bash
jq -r 'select(.fields.message | test("再接続|欠損|閾値超過"))' logs/collector.log.*
```

## レイテンシ計測の約束

`MessageTrace` は 2 種類の時計を明確に分けている。混同すると計測が壊れるため、
新しいコードでも必ずこの区別を守ること。

| 用途 | 使う時計 | フィールド |
|---|---|---|
| 取引所 → 自プロセスの遅延 | wall clock（NTP 同期前提） | `exchange_ts_ms` / `received_wall_ms` |
| 自プロセス内の区間計測 | monotonic (`Instant`) | `received_instant` / `normalized_instant` / `evaluated_instant` |

`exchange_to_local_ms()` が負になった場合は時計ズレの兆候として警告ログを出す
（`clock_skew_warnings`）。

`MessageTrace` は WS メッセージを受け取った**直後・パース前**に生成する。

## CSV カラム

| カラム | 説明 |
|---|---|
| `timestamp_ms` | 計算時刻（wall clock, ms epoch） |
| `symbol` | BTC / ETH / SOL / HYPE |
| `dex_a` / `dex_b` | 比較した DEX（`Dex` の宣言順に正規化。ペアごとに 1 行） |
| `mid_a` / `mid_b` | 各 DEX の mid 価格 |
| `best_bid_a` / `best_ask_a` | DEX A の最良気配 |
| `best_bid_b` / `best_ask_b` | DEX B の最良気配 |
| `raw_spread_bps` | mid ベースの乖離。正なら A が高い |
| `executable_spread_bps` | 実際に取れる方向の乖離（2 方向のうち有利な方） |
| `vwap_spread_bps` | 想定ノーショナルでの VWAP ベース乖離（深さ不足なら空） |
| `depth_a_bps10` / `depth_b_bps10` | 10bps 以内に収まる数量（bid/ask の薄い方） |
| `staleness_delta_ms` | A の受信時刻 − B の受信時刻 |
| `latency_a_ms` / `latency_b_ms` | 各 DEX の取引所 → 受信の遅延 |
| `pipeline_latency_us` | 内部処理時間（受信 → 判定完了, マイクロ秒） |

bps は基準価格に「両 mid の中点」を使う。A/B を入れ替えても符号が反転するだけで
絶対値が変わらないようにするため。

`executable_spread_bps` は有利な方向の値のみを持つが、方向自体は `mid_a` と `mid_b`
の大小（= 高い方で売る）から復元できる。`best_bid/ask` 4 列も残しているので、
両方向の値は CSV から再計算できる。

`staleness_delta_ms` が大きい行は「片方だけデータが古い」ことによる見かけ上の乖離を
含む。フェーズ1 完了後の分析では、まずこれで絞り込んでから乖離幅を集計すること。

## 設定

`config/collector.toml` を参照。記載しなかった項目はコード側の既定値
（`crates/config/src/lib.rs`）が使われ、未知のキーがあれば起動時にエラーになる。

主な項目:

| 項目 | 既定 | 意味 |
|---|---|---|
| `general.orderbook_depth` | 20 | 取得する板のレベル数 |
| `general.vwap_notional_usd` | 1000 | VWAP 乖離の想定ノーショナル（0 で無効） |
| `recording.csv_mode` | `all` | `all` / `sampled` / `threshold` |
| `dex.*.reconnect_max_attempts` | 10 | 0 で無制限 |
| `dex.edgex.resync_interval_secs` | 300 | 定期再購読で板の整合性を突き合わせる間隔 |
| `dex.aster.resync_backoff_ms` | 5000 | 板再初期化の最小間隔（IP ban 回避。必ず効かせる） |
| `dex.aster.reconnect_before_hours` | 23 | 24 時間の強制切断前に能動的に張り直す |
| `dex.lighter.keepalive_interval_secs` | 60 | クライアント側 keepalive（2 分未満必須） |
| `dex.lighter.use_testnet` | false | testnet に切り替える |
| `dex.*.excluded_symbols` | `[]` | その DEX で購読しない銘柄 |
| `monitoring.latency_warn_threshold_ms` | 500 | 超過時に警告ログ |

DEX ごとに `excluded_symbols` で銘柄を落とせる。市場が存在しない組み合わせは
これで除外する（除外しなくても板が来ないだけで異常にはならない）。

### DEX ごとの REST 依存

| DEX | 板の取得方式 | REST の要否 |
|---|---|---|
| Hyperliquid | 全量スナップショット配信 | 不要 |
| edgeX | 購読時スナップショット + 差分 | contractId 解決のみ（起動時 1 回、手動指定で回避可） |
| Aster | 差分更新 | **必須**。初期化時と再同期時に `/fapi/v1/depth` |
| Lighter | 購読時スナップショット + 差分 | **不要**（WS 一本化） |

Aster だけは構造的に REST が外せない。差分方式のため基準スナップショットを REST でしか
取得できず、`pu` の連続性が崩れるたびに再取得が要る。ここが IP ban のリスク源なので、
再取得は `resync_backoff_ms` で**全銘柄まとめて**間隔を空けている（ban は IP 単位で、
銘柄ごとのバックオフでは足りないため）。

### DEX ごとの keepalive 方式

4 DEX で ping/pong の仕組みがすべて異なる。共通化せず各 crate で個別に実装している。

| DEX | 方式 |
|---|---|
| Hyperliquid | クライアントが `{"method":"ping"}` を定期送信 |
| edgeX | サーバーが**アプリ層の JSON** `{"type":"ping","time":"..."}` を送信 → pong を返す |
| Aster | サーバーが**WS プロトコルの ping frame** を 5 分ごと送信 → 15 分以内に pong 必須 |
| Lighter | **クライアントが 2 分に 1 回以上**フレームを送る責任がある |

## 各 DEX の実装メモ

### Hyperliquid

- `wss://api.hyperliquid.xyz/ws` に `{"method":"subscribe","subscription":{"type":"l2Book","coin":"BTC"}}`
- `l2Book` は**毎回フルスナップショット**。差分再構築もシーケンス番号の欠損検知も不要
  （API にシーケンス番号が無い）。
- `data.time` が取引所タイムスタンプ。
- 無通信が続くと切断されるため、`{"method":"ping"}` を定期送信する。

### edgeX

- `wss://edgex-quote-prod-v2.edgex.exchange/api/v1/public/ws` に
  `{"type":"subscribe","channel":"depth.<contractId>.<level>"}`
- **差分更新（`dataType: "Changed"`）**が届くため、ローカルで板を再構築する。
  数量 0 は削除。`startVersion` / `endVersion` の連続性で欠損を検知し、
  欠損時は再購読して板を作り直す。
- 板のズレは静かに進行するため、3 段構えで守っている:
  ①バージョン連続性チェック ②スナップショット受信時の突き合わせ（ズレた数を警告ログに出す）
  ③定期再購読（`resync_interval_secs`）
- contractId は起動時に REST メタデータ（`metadata_url`）から解決し、
  `[dex.edgex.contract_ids]` の手動指定が常に優先される。

> **注意**: edgeX の WS 仕様は設計書時点で「未確定事項」であり、本実装の型は公開
> ドキュメントに基づく想定形式。パーサは配列形式・オブジェクト形式の価格レベル、
> 文字列/数値どちらのバージョン表記も受け付けるようにしてあるが、実 API と
> 突き合わせて差異があれば `crates/dex-edgex/src/message.rs` の固定サンプルと
> テストを更新すること。銘柄表記・contractId・testnet の有無も要確認のまま。

### Aster

- WS: `wss://fstream.asterdex.com/stream?streams=btcusdt@depth@100ms/...`（結合ストリーム）
- REST: `https://fapi.asterdex.com/fapi/v1/depth?symbol=BTCUSDT&limit=1000`
- **Binance 系の差分更新方式**。板の初期化は設計書の順序を厳守している:
  ①購読してイベントをバッファ ②REST スナップショット取得 ③`u < lastUpdateId` を破棄
  ④最初のイベントは `U <= lastUpdateId <= u` ⑤以降 `pu` == 直前の `u` を検証
- 数量は相対変化ではなく**絶対数量**（上書き）。0 は削除。ローカル板に無い価格の
  削除イベントは正常系として無視する。
- 連続性が崩れたら `resync_backoff_ms` を挟んで REST から作り直す。
- 24 時間で強制切断されるため `reconnect_before_hours`（既定 23h）で能動的に張り直す。

> **注意**: Spot 用の `sapi` ではなく Futures/Perp 用の `fapi` を使うこと。また
> Pro Mode（CLOB）と Simple Mode（ALP プール）があり、アービトラージ対象は Pro Mode の
> CLOB のみ。`fapi` が Pro Mode の板を返すことは実データで確認すること。

### Lighter

- WS: `wss://mainnet.zklighter.elliot.ai/stream`（testnet あり）
- **REST は一切使わない。** 板もシンボルマッピングも WS で完結する。
- 銘柄は文字列ではなく **market_index（数値）** で識別する。起動時に
  `market_stats:all` を購読し、`symbol` と `market_id` からマッピングを**動的に**
  構築してから `order_book:{MARKET_INDEX}` を購読する（ハードコードしない）。
  `market_stats` は流れ続けるため、マッピングの変化にも自動追随する。
- 購読時にスナップショット、以降は差分。順序検証は `begin_nonce` / `nonce` で行う。
  **`offset` は使わない**（API サーバーに紐づく値で、再接続で大きく変動するため）。
- クライアント側が 2 分に 1 回以上フレームを送らないと切断されるので、
  `keepalive_interval_secs`（既定 60 秒）で `{"type":"ping"}` を送る。
- 解決できなかった銘柄（その DEX に市場が無い場合）は警告を出して**スキップ**する。
  異常終了はしない。

> **注意**: Lighter の taker/maker 手数料は `market_stats` に含まれない。フェーズ2 の
> 利益判定を実装する段階で REST から起動時 1 回だけ取得する想定。フェーズ1 では不要。

## テスト

```bash
cargo test --workspace     # 166 tests
cargo clippy --workspace --all-targets
```

外部ネットワークには一切アクセスしない（WS/HTTP サーバはテスト内で 127.0.0.1 に立てる）。

- `core-types`: `vwap_for_size` / `max_size_within_slippage` の境界値（深さ不足、
  ちょうど食い切り、板が空、サイズ 0 以下）
- `market-data`: ペア列挙（N(N-1)/2・正準順序・重複除去）、板が無いペアのスキップ
- 各 DEX: 実レスポンス形式の固定サンプルによるパーサテスト、ローカル WS サーバを
  立てた購読 → 受信 → 再接続 → 欠損検知 → 再同期の通しテスト
- `dex-edgex`: 差分再構築（挿入・削除・連続適用・バージョン欠損・ズレ検出・トリム）
- `dex-aster`: `U`/`u`/`pu` の連続性検証、スナップショット前のバッファリング、
  数量 0 による削除、存在しないレベルの削除、バッファ上限、REST 再同期
- `dex-lighter`: `begin_nonce`/`nonce` の連続性検証、`offset` が飛んでも壊れないこと、
  market_index の動的解決、解決できない銘柄のスキップ
- `bin/collector`: 生 JSON → 価格差計算 → CSV 1 行までの統合テスト

## フェーズ1 完了後に分析すること

収集した CSV から集計し、フェーズ2 の判定ロジック設計に使う:

1. 銘柄別・時間帯別の乖離幅の分布
2. 手数料（両 DEX の taker 料率合計）を上回る乖離の発生頻度
3. **乖離の継続時間**（検知から解消まで何 ms か）← 最重要
4. `staleness_delta_ms` が大きい行を除外したとき、真の乖離がどれだけ残るか
5. DEX 別のレイテンシ分布と時間帯変動
6. **DEX ペアごとの `staleness_delta_ms` の分布を比較**し、鮮度差で説明できてしまう
   乖離を除外したうえで、真に利益機会がありそうなペアを特定する
