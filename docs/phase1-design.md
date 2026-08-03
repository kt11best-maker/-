# フェーズ1 詳細設計書 — Market Data収集基盤

> 全体仕様は `perp-arbitrage-bot-spec.md` を参照。本書はフェーズ1（土台とデータ収集）の実装指示書。

## フェーズ1のゴール

**実弾を一切使わず**、HyperliquidとedgeXの板データを24時間収集し、両DEX間の価格乖離がどの程度の頻度・幅で発生するかを実データで把握できる状態にする。

このフェーズで**実装しないもの**: 発注機能、署名ロジック、状態機械、キルスイッチ、証拠金管理。

### 完了条件

1. 4銘柄（BTC/ETH/SOL/HYPE）について、両DEXの板上位Nレベルをリアルタイム取得できている
2. 両DEXの価格差が計算され、JSONログとCSVの両方に出力されている
3. レイテンシ（取引所→受信、内部処理区間）が計測・記録されている
4. WS切断時に自動再接続し、切断イベントがログに残る
5. 24時間連続稼働してもメモリリーク・ファイル肥大化が起きない

---

## 1. crate構成（フェーズ1で作るもの）

```
perp-arb-bot/
├── Cargo.toml (workspace)
├── crates/
│   ├── core-types/       # 共通型（本フェーズで確定させる）
│   ├── dex-traits/       # DEX共通インターフェース(trait)
│   ├── dex-hyperliquid/  # Hyperliquid Market Data部分のみ
│   ├── dex-edgex/        # edgeX Market Data部分のみ
│   ├── market-data/      # 板集約・価格差計算
│   ├── recorder/         # JSONログ + CSV出力
│   └── config/           # 設定ファイル読み込み
└── bin/
    └── collector/        # フェーズ1用の実行バイナリ
```

※ `divergence` / `execution` / `risk` / `persistence` / `notifier` はフェーズ2以降。

---

## 2. core-types の型定義

数値型は **`rust_decimal::Decimal`** を使用する（`f64`は丸め誤差のため不可）。

```rust
use rust_decimal::Decimal;
use std::time::Instant;

/// 価格。Quantityとの取り違えを防ぐためnewtypeで包む
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Price(pub Decimal);

/// 数量
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Quantity(pub Decimal);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dex {
    Hyperliquid,
    EdgeX,
    // 将来: Dydx
}

/// 型安全優先でenum固定。銘柄追加時はここを変更する
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Symbol {
    Btc,
    Eth,
    Sol,
    Hype,
}

impl Symbol {
    /// 各DEXでのシンボル表記の違いを吸収する
    /// 例: Hyperliquidでは "BTC"、edgeXでは "BTCUSD" など（実APIで要確認）
    pub fn to_dex_symbol(&self, dex: Dex) -> &'static str { /* ... */ }
    pub fn from_dex_symbol(dex: Dex, s: &str) -> Option<Symbol> { /* ... */ }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side { Bid, Ask }

/// 板の1レベル
#[derive(Debug, Clone, Copy)]
pub struct Level {
    pub price: Price,
    pub quantity: Quantity,
}

/// 正規化された板スナップショット
#[derive(Debug, Clone)]
pub struct OrderBook {
    pub dex: Dex,
    pub symbol: Symbol,
    pub bids: Vec<Level>,  // 高い順にソート済み
    pub asks: Vec<Level>,  // 安い順にソート済み
    pub trace: MessageTrace,
}

impl OrderBook {
    pub fn best_bid(&self) -> Option<Price> { self.bids.first().map(|l| l.price) }
    pub fn best_ask(&self) -> Option<Price> { self.asks.first().map(|l| l.price) }
    pub fn mid(&self) -> Option<Price> { /* (best_bid + best_ask) / 2 */ }

    /// 指定数量を約定させた場合の平均約定価格（スリッページ計算の基礎）
    /// 板の深さが足りない場合はNoneを返す
    pub fn vwap_for_size(&self, side: Side, size: Quantity) -> Option<Price> { /* ... */ }

    /// 基準価格からmax_slippage_bps以内に収まる最大数量
    pub fn max_size_within_slippage(&self, side: Side, max_slippage_bps: u32) -> Quantity { /* ... */ }
}
```

### MessageTrace（レイテンシ計測用）

```rust
#[derive(Debug, Clone, Copy)]
pub struct MessageTrace {
    /// 取引所がメッセージ内に埋め込んだ生成時刻（wall clock, ms epoch）
    /// APIが提供しない場合はNone
    pub exchange_ts_ms: Option<u64>,
    /// 受信時のSystemTime（exchange_ts_msとの比較用。NTP同期前提）
    pub received_wall_ms: u64,
    /// 受信時のmonotonic instant（内部区間計測の起点）
    pub received_instant: Instant,
    /// パース・正規化完了時点
    pub normalized_instant: Option<Instant>,
    /// 価格差判定完了時点
    pub evaluated_instant: Option<Instant>,
}

impl MessageTrace {
    /// 取引所→自プロセスの遅延（wall clock比較）
    /// 負の値になる場合は時計ズレの兆候として警告ログを出す
    pub fn exchange_to_local_ms(&self) -> Option<i64>;
    /// 受信→正規化完了の処理時間（monotonic）
    pub fn normalize_latency(&self) -> Option<std::time::Duration>;
    /// 受信→判定完了の処理時間（monotonic）
    pub fn total_pipeline_latency(&self) -> Option<std::time::Duration>;
}
```

**重要**: 取引所との時刻比較には必ずwall clock、自プロセス内の区間計測には必ず`Instant`（monotonic）を使う。混同しないこと。

---

## 3. dex-traits（共通インターフェース）

将来dYdXを追加する際に、このtraitを実装したcrateを1つ足すだけで済む設計にする。

```rust
#[async_trait]
pub trait MarketDataSource: Send + Sync {
    fn dex(&self) -> Dex;

    /// 指定銘柄の板ストリームを開始し、正規化済みOrderBookをchannelに流す
    async fn subscribe_orderbooks(
        &self,
        symbols: &[Symbol],
        depth: usize,
        tx: mpsc::Sender<OrderBook>,
    ) -> Result<(), MarketDataError>;

    /// 接続状態（キルスイッチ・監視用）
    fn connection_status(&self) -> ConnectionStatus;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connected,
    Reconnecting { attempts: u32 },
    Disconnected,
}
```

---

## 4. dex-hyperliquid / dex-edgex（Market Data部分のみ）

各crateで実装すること:

1. WebSocket接続（`tokio-tungstenite`）
2. 板の購読メッセージ送信（各DEXのAPI仕様に従う）
3. 受信メッセージのパース → `OrderBook`への正規化
4. **受信した瞬間に`MessageTrace`を生成**（パース前に`Instant::now()`と`SystemTime::now()`を取る）
5. 自動再接続（指数バックオフ、上限付き）
6. シーケンス番号/更新IDの欠損検知（APIが提供する場合。欠損時は再購読して板を作り直す）
7. Ping/Pong等のkeepalive処理

### 実装前に確認が必要な事項

- 各DEXのWS購読方式（差分更新か全量スナップショットか）
- メッセージ内に取引所側タイムスタンプが含まれるか
- レートリミット・接続数制限
- 銘柄表記の違い（`Symbol::to_dex_symbol`の実装に必要）
- edgeXのtestnet提供有無

> **注**: 差分更新方式の場合、ローカルで板を再構築する必要がある。この再構築ロジックにバグがあると板が徐々にずれていくため、定期的に全量スナップショットで整合性チェックを行う仕組みを入れること。

---

## 5. market-data（板集約・価格差計算）

```rust
/// 銘柄ごとに全DEXの最新板を保持する共有状態
pub struct BookStore {
    // (Dex, Symbol) -> OrderBook
    books: DashMap<(Dex, Symbol), OrderBook>,
}

/// 2つのDEX間の価格差スナップショット
#[derive(Debug, Clone)]
pub struct DivergenceSnapshot {
    pub symbol: Symbol,
    pub dex_a: Dex,
    pub dex_b: Dex,
    pub mid_a: Price,
    pub mid_b: Price,
    /// mid価格ベースの乖離（bps）。符号でどちらが高いか判別
    pub raw_spread_bps: Decimal,
    /// best bid/askベースの「実際に取れる」乖離（bps）
    /// = (A.best_bid - B.best_ask) など、実際の約定方向を考慮した値
    pub executable_spread_bps: Decimal,
    /// 想定サイズで約定した場合のVWAPベース乖離（bps）
    pub vwap_spread_bps: Option<Decimal>,
    /// 両DEXの板の鮮度差（片方だけ古いデータだと見かけの乖離が生じる）
    pub staleness_delta_ms: i64,
    pub computed_at_wall_ms: u64,
    pub trace_a: MessageTrace,
    pub trace_b: MessageTrace,
}
```

**計算のポイント**:

- 単純なmid価格の差だけでなく、**実際に約定可能な方向での差**（A買い×B売り、A売り×B買いの両方向）を計算すること
- `staleness_delta_ms`は極めて重要。片方のDEXのデータだけが古いと「見かけ上の乖離」が発生するため、これを記録しないと後の分析で真の乖離と区別できない
- 板が更新されるたびに再計算する（イベント駆動）。定期ポーリングではなく、いずれかのDEXの板更新をトリガーにする

---

## 6. recorder（ログ・CSV出力）

### JSONログ（tracing）

```rust
let file_appender = tracing_appender::rolling::daily("logs", "collector.log");
let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
tracing_subscriber::fmt()
    .with_writer(non_blocking)
    .with_ansi(false)
    .json()
    .init();
```

記録するイベント:
- WS接続確立・切断・再接続（DEX別）
- パースエラー、想定外のメッセージ形式
- シーケンス欠損検知
- レイテンシ異常（閾値超過時のみ。全メッセージを出すとログが膨大になる）
- 時計ズレの疑い（`exchange_to_local_ms`が負、または急変した場合）

### CSV出力（分析用）

価格差スナップショットは別途CSVに出力する。1行1スナップショット。

**カラム定義**:

| カラム | 説明 |
|---|---|
| `timestamp_ms` | 計算時刻（wall clock, ms epoch） |
| `symbol` | BTC / ETH / SOL / HYPE |
| `dex_a` / `dex_b` | 比較したDEX |
| `mid_a` / `mid_b` | 各DEXのmid価格 |
| `best_bid_a` / `best_ask_a` | DEX Aの最良気配 |
| `best_bid_b` / `best_ask_b` | DEX Bの最良気配 |
| `raw_spread_bps` | mid価格ベースの乖離 |
| `executable_spread_bps` | 実際に取れる方向の乖離 |
| `vwap_spread_bps` | 想定サイズでのVWAPベース乖離 |
| `depth_a_bps10` / `depth_b_bps10` | 各DEXで10bps以内に収まる数量（板の厚さ指標） |
| `staleness_delta_ms` | 両DEXのデータ鮮度差 |
| `latency_a_ms` / `latency_b_ms` | 各DEXの取引所→受信の遅延 |
| `pipeline_latency_us` | 内部処理時間（マイクロ秒） |

**出力方針**:
- 銘柄ごとに日次でファイル分割（例: `data/2026-08-02_BTC.csv`）
- 24時間分の全更新を記録するとファイルが巨大になるため、書き込み頻度は設定可能にする（全件記録 / N ms毎にサンプリング / 乖離が閾値超過時のみ、を切替可能に）
- 初期は**全件記録**で開始し、ファイルサイズを見てから調整する

---

## 7. config（設定ファイル）

TOML形式。閾値をコード変更なしで調整できるようにする。

```toml
[general]
symbols = ["BTC", "ETH", "SOL", "HYPE"]
orderbook_depth = 20          # 取得する板のレベル数

[dex.hyperliquid]
enabled = true
ws_url = "..."
reconnect_max_attempts = 10
reconnect_base_delay_ms = 500

[dex.edgex]
enabled = true
ws_url = "..."
reconnect_max_attempts = 10
reconnect_base_delay_ms = 500

[recording]
csv_dir = "data"
log_dir = "logs"
csv_mode = "all"              # all | sampled | threshold
csv_sample_interval_ms = 100  # csv_mode = "sampled" の場合
csv_threshold_bps = 5         # csv_mode = "threshold" の場合

[monitoring]
latency_warn_threshold_ms = 500
staleness_warn_threshold_ms = 1000
```

---

## 8. bin/collector（実行バイナリ）

```
起動
 ├─ config読み込み
 ├─ ログ初期化（tracing → JSON file）
 ├─ BookStore初期化
 ├─ tokio::spawn: Hyperliquid WS購読タスク → mpsc::Sender<OrderBook>
 ├─ tokio::spawn: edgeX WS購読タスク       → mpsc::Sender<OrderBook>
 ├─ tokio::spawn: 集約タスク
 │    受信 → BookStore更新 → DivergenceSnapshot計算 → CSV writer へ送信
 ├─ tokio::spawn: CSV writerタスク（バッファリングして定期flush）
 ├─ tokio::spawn: 監視タスク（接続状態・レイテンシ異常を定期チェックしログ出力）
 └─ SIGINT/SIGTERM ハンドリング → CSVをflushして正常終了
```

**設計上の注意**:
- CSV書き込みはWS受信タスクと分離し、ディスクI/Oが受信処理をブロックしないようにする
- 各タスクはpanicしても他タスクを巻き込まないよう、`JoinHandle`を監視して再起動するsupervisorパターンを検討
- 24時間稼働前提のため、`Vec`等の無制限な蓄積を作らない（メモリリーク防止）

---

## 9. テスト方針

- `OrderBook::vwap_for_size` / `max_size_within_slippage` は**必ずユニットテストを書く**（スリッページ計算はフェーズ2以降の判定ロジックの基礎になるため、ここのバグは致命的）
- 板の差分更新による再構築ロジックも、既知の入力→期待される板状態のテストを用意する
- 各DEXのWSレスポンスのサンプルJSONを固定データとして持ち、パーサーのテストに使う

---

## 10. フェーズ1完了後に分析すること

収集したCSVから以下を集計し、フェーズ2の判定ロジック設計に使う:

1. 銘柄別・時間帯別の乖離幅の分布（ヒストグラム）
2. 手数料（両DEXのtaker料率合計）を上回る乖離の発生頻度
3. 乖離の継続時間（検知してから解消されるまで何ミリ秒か）← **最重要**。個人環境のレイテンシで間に合うかの判断材料
4. `staleness_delta_ms`が大きい時の乖離を除外した場合、真の乖離がどれだけ残るか
5. DEX別のレイテンシ分布と、時間帯による変動

---

## 11. 未確定事項（実装しながら埋める）

- 各DEXのWS API仕様詳細（購読形式、タイムスタンプの有無、シーケンス番号の有無）
- 銘柄表記のマッピング（`Symbol::to_dex_symbol`）
- edgeXのtestnet提供有無
- 各DEXのtaker手数料率（フェーズ2の判定ロジックで必要）
