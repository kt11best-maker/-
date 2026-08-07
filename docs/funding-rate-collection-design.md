# ファンディングレート収集 追加設計指示書（フェーズ1への追加）

> 全体仕様は `perp-arbitrage-bot-spec.md`、フェーズ1詳細は `phase1-design.md`、4 DEX拡張は `aster-lighter-integration-design.md` を参照。本書はフェーズ1にファンディングレート収集を追加する指示書。

## 目的と背景

将来的に**ファンディングレート裁定**（DEX間のファンディングレート差を取る戦略）を検討する可能性があるため、フェーズ1の段階でファンディングレートも並行収集しておく。

価格乖離裁定と違い、ファンディング裁定は速度勝負ではないため（精算が数時間ごと）、個人環境でも構造的に参加しやすい。既存の執行基盤・リスク管理はそのまま流用できる。

**重要**: 本追加はあくまで「データを取っておく」ためのもの。フェーズ1の主目的（価格乖離データの収集）を阻害しないこと。板データの収集より優先度は低く、ファンディング取得に失敗しても板の収集は継続すること。

---

## 1. 設計方針

### 1.1 板データとは独立したパイプラインにする

ファンディングレートは板と更新頻度が全く異なる（板は数十ms〜数百ms、ファンディングは数秒〜数分）。既存の`DivergenceSnapshot`のCSVに混ぜると、ほぼ同じ値が大量に重複して記録され無駄が多い。

**別チャンネル・別CSVファイルとして実装すること。**

```
data/2026-08-06_BTC.csv           # 既存: 価格乖離スナップショット
data/2026-08-06_BTC_funding.csv   # 追加: ファンディングレート
```

### 1.2 既存コードへの影響を最小化する

- `DivergenceSnapshot`の構造は変更しない
- `MarketDataSource` traitに新メソッドを追加するのではなく、**別traitとして定義**する（全DEXが同じ方式でファンディングを提供するとは限らないため）

---

## 2. core-typesへの追加

```rust
/// 正規化されたファンディングレート情報
#[derive(Debug, Clone)]
pub struct FundingRate {
    pub dex: Dex,
    pub symbol: Symbol,
    /// 現在のファンディングレート（1回の精算あたりの率。年率換算ではない）
    /// 符号: 正 = ロングがショートに支払う
    pub current_rate: Decimal,
    /// 予測レート（APIが提供する場合）
    pub predicted_rate: Option<Decimal>,
    /// 精算間隔（時間）。DEXによって1h / 8h など異なる
    pub interval_hours: Option<Decimal>,
    /// 次回精算時刻（wall clock, ms epoch）。APIが提供する場合
    pub next_funding_time_ms: Option<u64>,
    /// インデックス価格・マーク価格（取得できる場合）
    pub index_price: Option<Price>,
    pub mark_price: Option<Price>,
    pub trace: MessageTrace,
}

impl FundingRate {
    /// 年率換算したレート（%）。interval_hours が不明な場合は None
    /// = current_rate * (24 / interval_hours) * 365 * 100
    pub fn annualized_pct(&self) -> Option<Decimal>;
}
```

> **重要: 精算間隔の正規化**
>
> DEXによってファンディングの精算間隔が異なる（1時間ごと、8時間ごとなど）。**間隔が違うレートをそのまま比較してはいけない**。必ず`interval_hours`を記録し、比較・分析時は年率換算などで正規化すること。ここを取り違えると「8倍の差がある」と誤認する。
>
> 各DEXの精算間隔は実装時に必ず確認し、APIが返さない場合は設定ファイルに定数として持たせること。

---

## 3. dex-traits への追加

```rust
#[async_trait]
pub trait FundingRateSource: Send + Sync {
    fn dex(&self) -> Dex;

    /// ファンディングレートのストリームを開始する。
    /// 対応していないDEXでは実装しなくてよい（Option で扱う）。
    async fn subscribe_funding(
        &self,
        symbols: &[Symbol],
        tx: mpsc::Sender<FundingRate>,
    ) -> Result<(), MarketDataError>;
}
```

`MarketDataSource`とは別traitにすること。DEXによってはWSで提供されず、RESTポーリングになる可能性があるため。

---

## 4. DEX別の取得方法

### 4.1 Lighter（最も容易）

すでに`market_stats:all`チャンネルを購読しているため、**追加の接続は不要**。同じメッセージに含まれる以下のフィールドを使う。

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

- `current_funding_rate` → `FundingRate::current_rate`
- `funding_rate` → `predicted_rate`（意味を実装時に確認すること。予測値か直近確定値かでフィールドの割り当てを変える）
- `index_price` / `mark_price` もそのまま格納する

> 既存の`market_stats`購読処理から分岐させるだけで済む。最初にこのDEXから実装すると動作確認が早い。

### 4.2 Hyperliquid

`activeAssetCtx` などのWSチャンネル、または`info`エンドポイントの`metaAndAssetCtxs`でファンディングレートが取得できる。実装時に公式ドキュメントで以下を確認すること:

- WSで購読できるか、RESTポーリングが必要か
- 精算間隔（Hyperliquidは1時間ごとの精算だと理解しているが、必ず一次情報で確認すること）
- フィールド名と符号の向き

### 4.3 Aster

Binance互換のAPI設計のため、以下のいずれかで取得できる見込み:

- WS: `<symbol>@markPrice` ストリーム（mark price + funding rate が配信される。1秒 or 3秒間隔）
- REST: `/fapi/v1/premiumIndex`

**WSを優先すること**。RESTポーリングは既存のIP banリスクを増やすため避ける。

> `<symbol>@markPrice@1s` は既存の`<symbol>@depth@100ms`と同じ接続に相乗りできる（1接続200ストリームまで）。ただし**受信メッセージが1秒あたり10件までという制約**に注意。4銘柄 × (depth 10件/秒 + markPrice 1件/秒) が上限に触れないか計算すること。触れる場合は接続を分けるか、`@3s`版を使う。

### 4.4 edgeX

実装時に公式ドキュメントで確認すること。ticker系チャンネルに含まれる可能性が高い。

> **取得できない場合は無理に実装しないこと。** そのDEXは`FundingRateSource`を実装せず、ファンディングデータなしとして扱う。フェーズ1の主目的は板データであり、ここで詰まるべきではない。

---

## 5. CSV出力

既存の`recorder`に、ファンディング用の書き込み系統を追加する。銘柄ごと・日次でファイル分割する方針は既存と同じ。

**ファイル名**: `data/{YYYY-MM-DD}_{SYMBOL}_funding.csv`

**カラム定義**:

| カラム | 説明 |
|---|---|
| `timestamp_ms` | 記録時刻（wall clock, ms epoch） |
| `symbol` | BTC / ETH / SOL / HYPE |
| `dex` | 取得元DEX（**ペアではなく単一DEX**。ここが価格乖離CSVと違う点） |
| `current_rate` | 現在のファンディングレート（1回の精算あたり） |
| `predicted_rate` | 予測レート（取得できる場合、空欄可） |
| `interval_hours` | 精算間隔（時間） |
| `annualized_pct` | 年率換算（%）。**DEX間比較はこの列で行う** |
| `next_funding_time_ms` | 次回精算時刻（取得できる場合） |
| `index_price` | インデックス価格 |
| `mark_price` | マーク価格 |
| `exchange_ts_ms` | 取引所側タイムスタンプ |
| `latency_ms` | 取引所→受信の遅延 |

> **価格乖離CSVと構造が違う点に注意**: 価格乖離CSVは「1行 = 1ペアの比較」だが、ファンディングCSVは「1行 = 1DEXの状態」。ペア比較は分析時に行う。ファンディングは更新頻度が低く、ペアで持つと冗長になるため。

**記録頻度**:

- ファンディングレートは板ほど頻繁に変わらないため、**全件記録ではなく変化時のみ記録**する
- 具体的には、前回記録した値と`current_rate`が変わった場合のみ書き込む
- ただし値が変わらなくても、**最低5分に1回は記録**する（データの欠損とbotの停止を区別できるようにするため）

```toml
[recording.funding]
enabled = true
# 値が変化しなくても最低これだけの間隔で1行残す（秒）
heartbeat_interval_secs = 300
```

---

## 6. config への追加

```toml
[funding]
enabled = true

[funding.intervals_hours]
# APIが精算間隔を返さないDEXのためのフォールバック定数。
# 実装時に各DEXの公式ドキュメントで確認して埋めること。
hyperliquid = 1
edgex = 4
aster = 8
lighter = 1
```

> 上記の数値は**仮置き**である。必ず一次情報で確認してから設定すること。この値を間違えると年率換算が壊れ、分析結果が無意味になる。

---

## 7. テスト

- 各DEXのファンディングレスポンスのサンプルJSONを固定データとして持ち、パーサーのテストを書く
- `annualized_pct()` の計算テスト（精算間隔が異なるDEXで、同じ年率になるケースを検証する）
  - 例: 1時間ごと0.001% と 8時間ごと0.008% は同じ年率になること
- 変化時のみ記録するロジックのテスト（同じ値が連続したらスキップされること、heartbeat間隔を超えたら記録されること）

---

## 8. 実装優先順位

主目的である板データの収集を阻害しないよう、以下の順で進めること。

1. `core-types`に`FundingRate`と`FundingRateSource` traitを追加
2. **Lighter**のファンディング取得を実装（既存の`market_stats`購読から分岐するだけなので最も容易）
3. `recorder`にファンディングCSV出力を追加
4. Lighter単体で動作確認（CSVが正しく出るか、年率換算が妥当か）
5. **Aster**を追加（`@markPrice`ストリーム。レートリミットの計算を必ず行う）
6. **Hyperliquid**を追加
7. **edgeX**を追加（取得方法が不明なら後回し、または見送り）

> 全DEXが揃わなくても、取れるDEXから順に収集を始めてよい。ファンディング裁定の検討時に「一部DEXだけデータがある」状態でも、比較の出発点にはなる。

---

## 9. 実装前に確認すべき事項

- **各DEXの精算間隔**（1h / 4h / 8h など）← 最重要。間違えると分析が壊れる
- ファンディングレートの**符号の向き**（正 = ロングが支払う、という規約が全DEXで同じか）
- 各DEXがWSで提供するか、RESTポーリングが必要か
- AsterでmarkPriceストリームを追加した場合、1秒10メッセージ制限に触れないか
