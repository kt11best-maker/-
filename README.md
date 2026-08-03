# perp-arb-bot — フェーズ1（Market Data 収集基盤）

DEX パーペチュアル・アービトラージ Bot のフェーズ1 実装。
**実弾は一切扱わない。** Hyperliquid と edgeX の板を購読し、価格乖離とレイテンシを
24 時間収集して CSV / JSON ログに残すところまでが範囲。

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
│   ├── market-data/      # BookStore・価格差計算
│   ├── recorder/         # JSON ログ初期化・CSV 出力
│   └── config/           # TOML 設定
└── bin/collector/        # 実行バイナリ（タスク結線・supervisor・監視）
```

依存方向は一方向（`bin → crates`、`dex-* → dex-traits → core-types`）。dYdX を追加する
場合は `MarketDataSource` を実装した crate を 1 つ足し、`bin/collector` で結線するだけ
でよい。

## タスク構成

```
起動
 ├─ config 読み込み → ログ初期化 → BookStore 初期化
 ├─ spawn: Hyperliquid WS 購読タスク ─┐
 ├─ spawn: edgeX WS 購読タスク       ─┼→ mpsc<OrderBook>
 ├─ spawn: 集約タスク  ←─────────────┘  → mpsc<DivergenceSnapshot>
 ├─ spawn: CSV writer タスク（バッファリングして定期 flush）
 ├─ spawn: 監視タスク（接続状態・レイテンシ・スループット）
 └─ SIGINT/SIGTERM → CSV を flush して正常終了
```

- CSV 書き込みは WS 受信タスクと分離し、ディスク I/O が受信をブロックしない。
  集約 → CSV の送信はノンブロッキングで、詰まった場合は破棄件数を記録する
  （`snapshots_dropped`）。
- 各 Market Data タスクは supervisor 配下で動き、異常終了・panic しても他の DEX を
  巻き込まずに再起動する。
- 板の保持は「(DEX × 銘柄) の最新のみ」、edgeX のローカル板は購読レベル数でトリム、
  警告ログはレート制限付き。24 時間稼働でメモリ・ファイルが際限なく伸びない構成。

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
| `dex_a` / `dex_b` | 比較した DEX（常に A=hyperliquid, B=edgex に固定） |
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
| `monitoring.latency_warn_threshold_ms` | 500 | 超過時に警告ログ |

## 各 DEX の実装メモ

### Hyperliquid

- `wss://api.hyperliquid.xyz/ws` に `{"method":"subscribe","subscription":{"type":"l2Book","coin":"BTC"}}`
- `l2Book` は**毎回フルスナップショット**。差分再構築もシーケンス番号の欠損検知も不要
  （API にシーケンス番号が無い）。
- `data.time` が取引所タイムスタンプ。
- 無通信が続くと切断されるため、`{"method":"ping"}` を定期送信する。

### edgeX

- `wss://quote.edgex.exchange/api/v1/public/ws` に
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

## テスト

```bash
cargo test --workspace     # 101 tests
cargo clippy --workspace --all-targets
```

ネットワークには一切アクセスしない。

- `core-types`: `vwap_for_size` / `max_size_within_slippage` の境界値（深さ不足、
  ちょうど食い切り、板が空、サイズ 0 以下）
- `dex-hyperliquid` / `dex-edgex`: 実レスポンス形式の固定サンプルによるパーサテスト、
  ローカル WS サーバを立てた購読 → 受信 → 再接続 → 欠損検知 → 再購読の通しテスト
- `dex-edgex`: 差分再構築（挿入・削除・連続適用・バージョン欠損・ズレ検出・トリム）
- `bin/collector`: 生 JSON → 価格差計算 → CSV 1 行までの統合テスト

## フェーズ1 完了後に分析すること

収集した CSV から集計し、フェーズ2 の判定ロジック設計に使う:

1. 銘柄別・時間帯別の乖離幅の分布
2. 手数料（両 DEX の taker 料率合計）を上回る乖離の発生頻度
3. **乖離の継続時間**（検知から解消まで何 ms か）← 最重要
4. `staleness_delta_ms` が大きい行を除外したとき、真の乖離がどれだけ残るか
5. DEX 別のレイテンシ分布と時間帯変動
