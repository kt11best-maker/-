# perp-arb-bot — フェーズ1（Market Data 収集基盤）

DEX パーペチュアル・アービトラージ Bot の実装。**実弾は一切扱わない。**

**2 戦略**を同じ配管の上で動かす:

| 戦略 | 収益源 | 速度要求 | 保有時間 | 執行 |
|---|---|---|---|---|
| 価格差アービトラージ | DEX 間の一時的な価格乖離 | 極めて高い（数十〜数百 ms） | 秒〜分 | IOC 同時発注 |
| ファンディング裁定 | DEX 間のファンディングレート差 | 低い（数分〜数時間） | 時間〜日 | 指値でじっくり |

フェーズ1 で**稼働させる** DEX は **Hyperliquid + Lighter の 2 つ**。両者とも
**ファンディング精算間隔が 1 時間**で揃うため、レートを直接比較でき裁定の設計が
単純になる。edgeX / Aster / dYdX は実装済みだが**設定で無効化**している
（コードもテストも残す。再開は `enabled = true` だけでよい）。

現在の到達点: 板データとファンディングレートの 24 時間収集、両戦略の**判定
ロジック**（純粋関数）。発注・状態機械・永続化・通知はフェーズ3 以降で、
まだ存在しない。

## クイックスタート

```bash
cargo test --workspace          # ユニット + 統合テスト（ネットワーク不要）
cargo run --release -p collector -- --config config/collector.toml
```

停止は `Ctrl-C`（SIGINT）または `SIGTERM`。CSV を flush してから終了する。

出力先:

- `data/YYYY-MM-DD_<SYMBOL>.csv` — 価格差スナップショット（銘柄ごと・日次）
- `data/YYYY-MM-DD_<SYMBOL>_funding.csv` — ファンディングレート（**1 行 = 1 DEX**）
- `logs/collector.log.YYYY-MM-DD` — 運用イベント（JSON, 日次ローテーション）

ログレベルは `RUST_LOG=debug` で上書きできる（設定ファイルより優先）。

## crate 構成

```
perp-arb-bot/
├── crates/
│   ├── core-types/       # Price / Quantity / OrderBook / MessageTrace など共通型
│   ├── dex-traits/       # MarketDataSource / FundingRateSource trait・接続状態・バックオフ
│   ├── dex-hyperliquid/  # Hyperliquid の Market Data（WS + パース）
│   ├── dex-edgex/        # edgeX の Market Data（WS + 差分再構築 + contractId 解決）
│   ├── dex-aster/        # Aster の Market Data（WS 差分 + REST スナップショット）
│   ├── dex-lighter/      # Lighter の Market Data（WS 一本化・market_index 動的解決）
│   ├── dex-dydx/         # dYdX v4 の Market Data（Indexer WS・クロス板を許容）
│   ├── market-data/      # BookStore / FundingStore・ペア列挙・価格差計算
│   ├── strategy-traits/  # 戦略の共通インターフェース（TradeSignal / Strategy）
│   ├── strategy-price-arb/    # 価格差アービトラージの判定
│   ├── strategy-funding-arb/  # ファンディング裁定の判定
│   ├── risk/             # 戦略別の証拠金枠・多層キルスイッチ
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
 ├─ spawn: ファンディング収集タスク（対応 DEX のみ） ─→ mpsc<FundingRate>
 ├─ spawn: ファンディング CSV writer タスク
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

有効な DEX が N 個なら比較ペアは **N(N-1)/2 通り**（5 DEX なら 10 ペア）。ペアの
向きは `Dex` の宣言順（hyperliquid → edgex → aster → lighter → dydx）に正規化される
ため、CSV の `dex_a`/`dex_b` の並びと符号の意味は行ごとに変わらない。

板の更新はどれか 1 つの DEX でしか起きないので、**更新された DEX を含むペアだけ**を
再計算する（1 更新あたり N-1 行）。その銘柄の板がまだ揃っていないペア
（= その DEX に市場が無い、未受信）は**正常系として黙ってスキップ**する。
「全銘柄が全 DEX に存在する」前提は置かない。

> **CSV の行数に注意**: ペア数 × 銘柄数が増えると `csv_mode = "all"` のファイル増加が
> 速い。まず数十分だけ動かして実測し、24 時間分を見積もってから本稼働すること。
> 必要なら `sampled` に切り替える（設定のみで、コード変更は不要）。

## 2 戦略の判定ロジック

どちらも `strategy_traits::Strategy` を実装し、**I/O を持たない純粋関数**として
書いてある。同じ市場状態を渡せば同じシグナルが出るので、フェーズ2 のドライランで
収集済みデータを流し直して事後評価できる。

### 価格差アービトラージ（`strategy-price-arb`）

```text
実質乖離 = 生の乖離 − 手数料 − スリッページ − 安全マージン
```

- 生の乖離は既定で **VWAP ベース**（想定サイズを板に食い込ませた平均約定価格の差）。
  この場合スリッページは織り込み済みなので二重に引かない。
- **手数料は既定で 4 レグ**（建て + 決済 × 2 DEX）。捉えた乖離はいずれ決済するため、
  建ての 2 レグだけで判定すると過小評価になる（`count_exit_fees = false` で
  旧仕様の 2 レグ判定にもできる）。
- `staleness_delta_ms` が閾値を超えるペアは**判定前に落とす**。

### ファンディング裁定（`strategy-funding-arb`）

レートが**高い DEX でショート**、**低い DEX でロング**。両建てなので価格変動は
相殺され、レート差を精算ごとに受け取る。

```text
総損益(bps) = レート差 × 保有精算回数
            + (エントリー時の価格差 − エグジット時の価格差)   ← ベーシス項
            − 手数料（建て + 決済 = 4 レグ）− スリッページ − 安全マージン
```

**ベーシス項の扱いがこの戦略の肝。** perp 同士の両建てでは価格差は利益源ではなく
エントリー/エグジットのコスト（またはボーナス）として効く。2 DEX 間の価格差が
建てた時と決済した時で変わると、その差分がそのまま損益になる。

- 既定では**ベーシスをゼロとして評価**し、「価格差が不利な時は建てない」という
  フィルタとしてのみ使う（`max_adverse_basis_bps`）。収束を当てにすると、
  収束しなかった場合に想定が崩れるため。
- 有利なベーシスを利益に加算するには `count_favorable_basis = true` を明示する。
- 手数料が 1 精算のレート差を上回る場合、**回収に必要な最低精算回数**を計算し
  （`breakeven_intervals`）、`max_acceptable_breakeven_intervals` を超えたら棄却する。
- 精算間隔が異なる DEX 同士は既定で扱わない（1 精算あたりの比較が成立しないため）。
  扱う場合は年率換算（`FundingRate::annualized_bps`）で正規化する。

> **`expected_profit_bps` の意味が戦略で違う。** 価格差は「1 往復の純利益」、
> ファンディングは「**1 精算あたり**の純収益」。同じ土俵で比較してはいけない。
> 資金配分の判断には `TradeSignal::total_expected_profit_bps()`（保有期間全体に
> 換算した値）を使うこと。

### 資金配分とキルスイッチ（`risk`）

2 戦略が同じ証拠金プールを取り合うと、ファンディング裁定の長期ポジションが
価格差アービトラージの機会を潰す（またはその逆）。**枠を分離する。**

```toml
[allocation]
price_arb_pct = 0.30
funding_arb_pct = 0.40
reserve_pct = 0.30      # どちらの戦略も使えない緊急クローズ専用の余力
```

キルスイッチは**戦略ごとに独立して発動**できる。価格差が不調でもファンディングは
継続してよい場合があるため。Bot 全体の日次損失上限に達した場合のみ両方を止める。
証拠金維持率の悪化は戦略横断の事象なので全体停止として扱う。

### 手数料

**既定値を持たせていない。** 未設定の DEX を含むペアは、戦略がシグナルを出さずに
スキップする（確認前の値で利益判定が通らないようにするため）。`[fees.<dex>]` に
一次情報で確認した値を入れて初めて判定が動く。

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
| `book_crossed_a` / `book_crossed_b` | 各 DEX の板がクロス（bid >= ask）していたか |
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

`book_crossed_*` が `true` の行も同様に除外して集計すること。クロスした板の best 気配は
実際には取れないため、乖離が実在するように見えてしまう。dYdX ではクロスが構造上正常に
起こるので、この列は**発生頻度の計測そのもの**が目的でもある。

## ファンディングレート CSV

`data/YYYY-MM-DD_<SYMBOL>_funding.csv`。価格差 CSV とは**構造が違う**ので注意。

| | 価格差 CSV | ファンディング CSV |
|---|---|---|
| 1 行の意味 | 1 ペアの比較（`dex_a` / `dex_b`） | **1 DEX の状態**（`dex` 1 列） |
| 更新頻度 | 数十〜数百 ms | 数秒〜数分 |

ペア比較は分析時に行う。ファンディングは更新頻度が低く、ペアで持つと同じ値が
冗長に並ぶため。

| カラム | 説明 |
|---|---|
| `timestamp_ms` | 記録時刻（wall clock, ms epoch） |
| `symbol` / `dex` | 銘柄と取得元 DEX |
| `current_rate` | 現在のレート（**1 回の精算あたり**。年率ではない） |
| `predicted_rate` | 予測レート（提供する DEX のみ） |
| `interval_hours` | 精算間隔（時間）。未設定なら空欄 |
| `annualized_pct` | 年率換算（%）。**DEX 間比較はこの列で行う** |
| `next_funding_time_ms` | 次回精算時刻（提供する DEX のみ） |
| `index_price` / `mark_price` | インデックス価格・マーク価格 |
| `exchange_ts_ms` / `latency_ms` | 取引所側タイムスタンプと遅延 |

**記録は変化時のみ。** `current_rate` が前回と同じ行は書かない。ただし値が
変わらなくても `heartbeat_interval_secs`（既定 300 秒）ごとに 1 行残す
（**データの欠損と bot の停止を区別できるようにするため**）。

### 精算間隔の正規化（最重要）

DEX によってファンディングの精算間隔が異なる（1 時間ごと、8 時間ごとなど）。
**間隔が違うレートをそのまま比較してはいけない。** 1 時間ごと 0.001% と
8 時間ごと 0.008% は年率では同じ（8.76%）だが、`current_rate` は 8 倍違う。
ここを取り違えると「8 倍の差がある」と誤認する。

精算間隔に**既定値は持たせていない**。`[funding.intervals_hours]` に一次情報で
確認した値を入れて初めて `annualized_pct` が埋まる。未設定なら空欄のまま残り、
ファンディング裁定の判定からも（間隔不明として）外れる。推測値で埋めると年率換算が
壊れて分析が無意味になるため、手数料と同じ扱いにしてある。

### DEX ごとの対応状況

| DEX | 取得方法 | 追加の接続 |
|---|---|---|
| Hyperliquid | `activeAssetCtx` チャンネル | 不要（板と同じ接続） |
| Lighter | `market_stats:all`（既に購読済み） | 不要（受信済みメッセージから分岐） |
| edgeX | 未対応（取得方法が未確認） | — |
| Aster | 未対応（`@markPrice` で取れる見込み。§下記） | — |

ファンディングは板とは**独立したパイプライン**。取得に失敗しても板の収集は止まらない。
下流が詰まった場合はレートを**捨てる**（板の受信ループを 1 ms でも止めないため）。
フェーズ1 の主目的は板データの収集で、ファンディングはその邪魔をしない。

> Aster を有効化してファンディングも取る場合、`<symbol>@markPrice` は既存の
> `<symbol>@depth@100ms` と同じ接続に相乗りできるが、**受信メッセージが 1 秒
> あたり 10 件までという制約**がある。4 銘柄 × (depth 10 件/秒 + markPrice 1 件/秒)
> が上限に触れないか必ず計算すること。触れる場合は接続を分けるか `@3s` を使う。

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
| `strategy.price_arb.max_staleness_delta_ms` | 250 | 鮮度差がこれを超えるペアは判定しない |
| `strategy.funding_arb.max_adverse_basis_bps` | 2.0 | これ以上不利なベーシスでは建てない |
| `strategy.funding_arb.count_favorable_basis` | false | 有利なベーシスを利益に加算するか |
| `strategy.funding_arb.max_holding_hours` | 72 | 最大保有時間 |
| `funding.enabled` | true | ファンディング収集の有効/無効 |
| `funding.intervals_hours.<dex>` | なし | **未設定なら年率換算しない** |
| `recording.funding.heartbeat_interval_secs` | 300 | 値が変わらなくても残す間隔 |
| `dex.dydx.connected_timeout_secs` | 10 | `connected` を待つ上限 |
| `allocation.*` | 0.30/0.40/0.30 | 戦略別の証拠金枠（合計 1.0 以下） |
| `fees.<dex>.taker_bps` / `maker_bps` | なし | **未設定の DEX はシグナル対象外** |
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
| dYdX v4 | 購読時スナップショット + 差分 | **不要**（Indexer WS で完結） |

Aster だけは構造的に REST が外せない。差分方式のため基準スナップショットを REST でしか
取得できず、`pu` の連続性が崩れるたびに再取得が要る。ここが IP ban のリスク源なので、
再取得は `resync_backoff_ms` で**全銘柄まとめて**間隔を空けている（ban は IP 単位で、
銘柄ごとのバックオフでは足りないため）。

### DEX ごとの keepalive 方式

5 DEX で ping/pong の仕組みがすべて異なる。共通化せず各 crate で個別に実装している。

| DEX | 方式 |
|---|---|
| Hyperliquid | クライアントが `{"method":"ping"}` を定期送信 |
| edgeX | サーバーが**アプリ層の JSON** `{"type":"ping","time":"..."}` を送信 → pong を返す |
| Aster | サーバーが**WS プロトコルの ping frame** を 5 分ごと送信 → 15 分以内に pong 必須 |
| Lighter | **クライアントが 2 分に 1 回以上**フレームを送る責任がある |
| dYdX v4 | サーバーが**WS プロトコルの ping frame** を 30 秒ごと送信 → 10 秒以内に pong 必須 |

edgeX（アプリ層 JSON）と dYdX（プロトコル制御フレーム）は仕組みが別物なので、
同じコードで扱おうとしないこと。

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

### dYdX v4

- WS: `wss://indexer.dydx.trade/v4/ws`（testnet あり。公式に提供されているので
  フェーズ3 の機能検証に使える）
- **`connected` を受信してから購読する。** 接続直後に送ってはいけない。
- `{"type":"subscribe","channel":"v4_orderbook","id":"BTC-USD"}`。銘柄表記は
  ハイフン区切りの USD 建て。
- `subscribed` が全量スナップショット（受信時にローカル板を必ずリセットしてから
  作り直す）、以降は `channel_data` の差分。**`size` が 0 の価格レベルは削除。**
  順序検証は `message_id`。
- Ping は WS **プロトコルレベルの制御フレーム**（30 秒ごと、10 秒以内に pong）。

> #### 板がクロスすることがある（乖離判定に直結）
>
> dYdX は中央集権的なオーダーブックを持たないため、**bid が ask より高い
> （クロスした）板が観測されうる**。これは異常データではなく構造上正常に起こる。
>
> - クロスした板も**捨てずに**下流へ流す（`crossed_books` カウンタで頻度を計測）
> - CSV には `book_crossed_a` / `book_crossed_b` として残す
> - **クロスした板から計算した乖離はアービトラージ機会として扱わない**
>   （`strategy-price-arb` が除外する）

> #### Indexer のデータ鮮度
>
> Indexer はブロックチェーンの状態を追ってDBに反映する中間層で、真に正しい板
> （ブロックプロポーザーの mempool 内）とは差がある。**dYdX の板は構造的に
> 「少し古い」可能性がある。** `staleness_delta_ms` が dYdX を含む組み合わせで
> 系統的に大きくなっていないか必ず確認すること。大きい場合、dYdX との乖離の多くは
> 「見かけ上の乖離」である可能性が高い。
>
> 板メッセージに取引所側タイムスタンプが含まれないため `latency_ms` は空欄になる。
> 鮮度は `staleness_delta_ms` で見ること。

> **有効化前に確認すること**: `HYPE-USD` 市場の有無（無ければ
> `excluded_symbols = ["HYPE"]`）、taker 手数料率、Indexer WS のレートリミットと
> 同時接続数、板の深さが設定で変えられるか。

## テスト

```bash
cargo test --workspace     # 292 tests
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
  market_index の動的解決、解決できない銘柄のスキップ、既存の `market_stats` 購読から
  ファンディングが取れること（購読メッセージが増えないことも検証）
- `dex-dydx`: `connected` 前に購読しないこと、**size=0 による価格レベル削除**、
  `message_id` 欠損の検知と再購読、クロスした板が捨てられずカウントされること、
  プロトコルレベル ping への pong 応答
- `recorder`: 変化時のみ記録するロジック（同値の連続はスキップ、heartbeat 超過で記録）、
  精算間隔が違っても `annualized_pct` が一致すること
- `strategy-price-arb`: 手数料超え判定、鮮度差フィルタ、VWAP/best 気配の切替、
  板が薄い場合、手数料未設定時のスキップ、収束時の手仕舞い
- `strategy-funding-arb`: 手数料回収回数、不利ベーシスの棄却、有利ベーシスを
  既定で加算しないこと、精算間隔不一致の扱い、レート反転・保有上限での手仕舞い
- `risk`: 戦略枠の分離、枠超過の縮小承認、緊急クローズ余力の侵食検知、
  戦略別キルスイッチと全体停止の優先関係
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

ファンディング裁定側は、収集を実装したうえで以下を見る:

1. レート差の分布
2. **レート差が同符号で持続する時間**（最重要）。1 回の精算しか持たないなら手数料で消える
3. 手数料回収に必要な精算回数の分布、レート反転の頻度
4. **ファンディング差が有利な瞬間の価格差（ベーシス）の分布**と、一定時間内に
   ベーシスがどれだけ動くか。ベーシスの変動幅が大きいと、ファンディング収益が
   価格差の変動に埋もれる

## 未実装 / 保留

- **edgeX / Aster のファンディング取得**: edgeX は取得方法が未確認、Aster は
  `@markPrice` で取れる見込みだがレートリミットの計算が要る。どちらも現在は
  板のみ収集する（起動時に警告を出す）。
- **戦略の collector への結線**: フェーズ1 は板・ファンディングの収集のみ。
  シグナル記録はフェーズ2（ドライラン）で行う。
- **execution / persistence / notifier**: フェーズ3 以降。

### 実 API と突き合わせて確認すべきこと

この環境では外部への TLS 接続ができず（プロキシの証明書検証で弾かれる）、
**実 API のレスポンス形式は未検証**。パーサはいずれも複数の形式を受け付けるように
してあるが、実データと差異があれば各 crate の固定サンプルとテストを更新すること。

| 項目 | 場所 |
|---|---|
| 各 DEX の**精算間隔** ← 最重要 | `[funding.intervals_hours]` |
| ファンディングの**符号の向き**（正 = ロングが支払う、で全 DEX 統一か） | 各 dex crate |
| Lighter の `funding_rate` の意味（予測値か直近確定値か） | `dex-lighter/src/message.rs` |
| Hyperliquid `activeAssetCtx` のフィールド名 | `dex-hyperliquid/src/message.rs` |
| dYdX の `HYPE-USD` 市場の有無、`message_id` の連番の仕様 | `dex-dydx/` |
| 各 DEX の taker/maker 手数料率 | `[fees.<dex>]` |
