# DEXパーペチュアル・アービトラージBot 開発仕様書

## 1. プロジェクト概要

複数のオーダーブック型DEX（無期限先物 / perp）の価格を24時間監視し、同一銘柄で価格乖離が大きいDEX間でロング・ショートを自動売買するAI botを開発する。開発言語はRust。

- **戦略の主軸**: 瞬間的な価格乖離（板の歪み等、秒〜分単位で解消されるもの）を狙う
- **レバレッジ**: 使用しない
- **対象DEX（初期）**: Hyperliquid、edgeX
  - 今後、dYdXなどを追加予定
- **対象銘柄（初期）**: BTC、ETH、SOL、HYPE

## 2. サイジング・資金管理

- 1取引あたりのサイズ上限: 両DEXの証拠金プールのうち少ない方の **5%**
- 発注前に両DEXの板の厚さを確認し、その範囲内に収まる金額で取引する
- サイジング計算には安全マージン係数をかける（見せ板・競合による過大評価を防ぐため）
- 複数銘柄が同時にシグナル発火するケースを想定し、「1銘柄あたりの上限」と「全銘柄合計の上限」の2段階でキャップを設ける
- 証拠金プールの空き容量チェックは全銘柄共通で一元管理する（早い者勝ちで証拠金を食い尽くさないようにする）

## 3. 両建て発注ロジック

- **同時発注方式**: Leg1・Leg2を`tokio::join!`等でほぼ同時に発注する（順次発注だと待ち時間中に価格が変動するため）
- IOC（Immediate-or-Cancel）等、約定しないなら即キャンセルされる注文方式を使う
- 約定確認は「発注APIの成否」ではなく「WebSocket等の実際の約定イベント」で判断する

### 状態機械（HedgeState）

```rust
enum HedgeState {
    Idle,
    BothPending { leg1_id: OrderId, leg2_id: OrderId, sent_at: Instant },
    Hedged { leg1: Fill, leg2: Fill },
    PartialUnhedged { filled: Fill, filled_dex: Dex, since: Instant },
    Unwinding { position: Fill, close_order_id: OrderId },
}
```

- 片方だけ約定した場合（`PartialUnhedged`）は、約定した側を即座に成行/IOCでクローズする
- この経路に入った時点で「乖離益は諦めて損失最小化を優先する」動作と割り切る
- 部分約定で数量が完全一致しない場合に備え、Fill量ベースで差分を計算し超過分のみクローズする
- 状態遷移のたびにログ・永続化を行い、プロセスクラッシュ時も再起動後に不整合ポジションを検知できるようにする

## 4. 乖離検知・利益判定ロジック

```
実質乖離 = |Price_A - Price_B|
         - (Fee_A + Fee_B)
         - (Slippage_A + Slippage_B)
         - FundingCost_A→B
         - Buffer（安全マージン）
```

- 手数料はtaker料率で計算（IOC前提のため）
- スリッページは実際に発注するサイズに対応する板の食い込み分で計算する
- 瞬間的な乖離が主軸のため、ファンディングレート差の影響は小さい想定だが、ポジションが長引く場合の撤退ルール（タイムアウト手仕舞い）も用意する
- 実質乖離が最低利益閾値を上回った場合のみシグナルを発火する

## 5. キルスイッチ・リスク管理（多層構成）

1. **注文レベル**: 最大サイズ・最大許容スリッページ超過で発注ブロック。`PartialUnhedged`滞在時間が閾値を超えたら強制クローズ
2. **銘柄レベル**: 特定銘柄で`Unhedged`突入が短時間に連続発生したら、その銘柄の取引を一時停止
3. **DEXレベル**: WS切断・再接続の繰り返しやAPIレイテンシ異常でそのDEXへの新規発注を停止。証拠金維持率低下時も新規発注停止
4. **Bot全体レベル**: 1日の最大損失額（サーキットブレーカー）到達で全ポジションクローズ＋新規発注停止。「ソフト停止」と「ハード停止」の2段階を用意
- 発動条件・閾値は設定ファイルで外出しにする
- 発動時は必ず通知（Slack/Telegram等）を送る
- キルスイッチはMarket Data/Executionレイヤーとは独立したwatchdogタスクとして実装する

## 6. レイテンシ計測

- 3種類のタイムスタンプを記録する: ①取引所生成時刻（wall clock）、②WSメッセージ受信時刻、③バリデータ通過時刻
- 取引所⇔自プロセス間の遅延計測には**wall clock（NTP同期前提）**を使う
- 自プロセス内の処理区間（受信→検証→判定→発注）の計測には**monotonic clock（`std::time::Instant`）**を使う
- DEXごとにレイテンシを継続監視し、片方のDEXだけ受信が遅れることで生じる「見かけ上の乖離」を識別できるようにする

## 7. システムアーキテクチャ（レイヤー構成）

1. **Market Dataレイヤー**: 各DEXのWS接続、板・約定・ファンディングレートの正規化
2. **State/Aggregationレイヤー**: 銘柄ごとに両DEXの最新気配値を保持、価格差を計算
3. **Signal/Decisionレイヤー**: 実質乖離の算出、閾値判定
4. **Executionレイヤー**: DEXごとの発注クライアント、状態機械、ロールバック処理
5. **Risk/Position管理レイヤー**: 証拠金・ポジション監視、キルスイッチ

## 8. Rust crate構成（Cargo workspace）

```
perp-arb-bot/
├── Cargo.toml (workspace)
├── crates/
│   ├── core-types/       # Price, Fill, OrderId, Dex, Symbol等の共通型
│   ├── market-data/      # 各DEXのWS接続・板正規化
│   ├── dex-hyperliquid/  # Hyperliquid固有のAPI/WS/署名ロジック
│   ├── dex-edgex/        # edgeX固有のAPI/WS/署名ロジック
│   ├── divergence/       # 乖離検知・利益判定ロジック
│   ├── execution/        # 発注・状態機械・ロールバック
│   ├── risk/             # キルスイッチ・証拠金プール管理・サイジング
│   ├── persistence/      # 状態永続化・再起動時の復元
│   ├── notifier/         # Slack/Telegram通知
│   └── config/           # 設定ファイル読み込み
└── bin/
    └── bot/              # 上記crateを組み合わせたmain
```

- `dex-hyperliquid`と`dex-edgex`は個別crateとし、共通インターフェースはtraitで定義（将来dYdX追加時に`dex-dydx`を追加するだけで済む設計）
- 依存方向は一方向（riskがexecutionを呼ぶのはOKだが逆はNG）

## 9. ロギング

- `tracing` + `tracing-appender`でファイルにJSON形式出力（`rolling::daily`でローテーション）
- 記録すべきイベント: 状態遷移、発注・約定確認、乖離検知の判定結果、キルスイッチ発動、WS切断・再接続、APIエラー
- 機密情報（APIキー・署名）はログに出力しない

## 10. 検証方針（2段階）

1. **testnetでの機能検証**: 発注・署名・約定通知・状態機械・緊急クローズのフローが正しく動くかを検証（testnetの価格は実需を反映しないため、価格乖離の検証には使わない）
2. **mainnetデータでのドライラン**: 実際の板データに対しシグナルのみ記録（発注はしない）し、乖離の頻度・実質利益・閾値の妥当性を検証

## 11. セキュリティ（秘密鍵管理）

- 本番環境ではAPIキー・署名鍵をファイル/環境変数に直書きせず、**1Password CLI**経由で実行時に注入する
- `op run --env-file=".env.tpl" -- ./target/release/perp-arb-bot` の形で起動
- `.env.tpl`には実値ではなく`op://vault/item/field`形式の参照のみを記載
- サーバー環境では1Password Service Account（headless運用）の利用を想定。そのアクセストークン自体の管理も別途検討する

## 12. 実装優先順位

**フェーズ1: 土台とデータ収集（実弾なし）**
1. `core-types`の共通型設計
2. `dex-hyperliquid` / `dex-edgex`のMarket Data部分（WS接続・板受信・正規化）のみ実装
3. 両DEXの価格差をログ出力する簡易版を動かす
4. レイテンシ計測の仕組みを同時に組み込む

**フェーズ2: 判定ロジックの検証（ドライラン）**
5. `divergence`（乖離検知・利益判定ロジック）を実装し、シグナルのみ出す
6. ログを集計し、閾値・シグナル頻度を検証

**フェーズ3: 発注・状態機械（testnet）**
7. 両DEXに発注機能を追加（署名・注文送信・約定通知）
8. `execution`（HedgeState状態機械、同時発注、ロールバック）をtestnetで検証
9. `risk`（サイジング、板厚みチェック、キルスイッチ）を組み込む

**フェーズ4: 本番導入への足回り**
10. `persistence`（状態永続化・再起動復元）
11. `notifier`（通知）
12. 1Password CLI連携、`config`の外出し
13. ごく小さいサイズでmainnet実弾投入 → 段階的にサイズを引き上げ

## 13. 開発環境

- OS: macOS（Intel、Monterey 12.7.6）
- Homebrewは使用不可のため、`rustup`経由でRustをインストール済み
- Xcodeコマンドラインツールは導入済み
- エディタ: VS Code + rust-analyzer を推奨（未確定）

## 14. 未確定・要検討事項

- edgeXのtestnet有無・板の実流動性の確認
- 1Password Service Accountのアクセストークン自体の管理方法
- 観測性基盤（メトリクス収集・可視化ツールの選定）
- 各種閾値（最低利益閾値、キルスイッチの損失上限額、Unhedged強制クローズまでの時間等）の具体的な数値
