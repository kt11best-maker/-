//! ネットデルタの監視ループ。
//!
//! **キルスイッチと同じく、Market Data / Execution から独立したタスク**として
//! 動かす。板の受信が詰まっても、執行レイヤーが固まっても、ここは回り続けて
//! 「ずれていること」に気づけなければ意味がない。
//!
//! ```text
//! 定期実行（check_interval_secs ごと）
//!  ├─ 各 DEX の実ポジションを API で取得
//!  ├─ 銘柄ごとに合算し net_delta を算出
//!  ├─ bot の想定ポジションと比較し drift を算出
//!  └─ 閾値判定
//!      ├─ 許容範囲内   → 何もしない
//!      ├─ 警告閾値超過 → ログ + 通知
//!      ├─ 是正閾値超過 → リバランス（発注はフェーズ3）
//!      └─ critical    → ハード停止 / drift 超過 → ソフト停止
//! ```
//!
//! # 取得に失敗した DEX があればその周期は判定しない
//!
//! 「取得できなかった」を「ポジション無し」と混同すると、**片肺ポジションを
//! 中立だと誤認する**。1 つでも失敗したらその周期はスキップし、警告だけ出す。
//!
//! # 想定ポジションは監視開始前に復元しておくこと
//!
//! drift は「bot が持っているつもりの数量」との差なので、想定が空のまま実
//! ポジションがあると drift 超過（ソフト停止）になる。**これは正しい挙動**
//! （クラッシュ後の復元漏れを検知したいケースそのもの）だが、フェーズ3 で
//! 起動する際は永続化した想定ポジションを復元してから監視を始めること。
//!
//! # フェーズ1 では動かない
//!
//! 実ポジション取得（[`PositionSource`]）には認証付き API が必要で、発注機能の
//! 無いフェーズ1 では実装が無い。collector は「ソースが 1 つも無い」ことを
//! 起動時に報告し、このタスクを起動しない。構造だけ用意しておき、フェーズ3 で
//! 各 DEX の実装を足せば有効になる。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use config::NetDeltaConfig;
use core_types::{now_wall_ms, Price, Symbol};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::killswitch::KillSwitch;
use crate::net_delta::{NetDeltaStatus, PositionManager};
use crate::position::{DexPosition, PositionSource};

/// 複数タスクから触るキルスイッチ。
///
/// 監視タスクが停止させ、発注側（フェーズ3）が状態を読む。ロック中に await
/// しないこと（`std::sync::Mutex` を使っているのはそのため）。
pub type SharedKillSwitch = Arc<Mutex<KillSwitch>>;

/// ノーショナル換算に使う価格の供給元。
///
/// フェーズ3 では `BookStore` の mid か、ファンディング経由のマーク価格を
/// 渡す想定。**価格が取れない場合は `None` を返すこと**（0 を返してはいけない。
/// ずれを中立だと誤認する）。
pub trait MarkPrices: Send + Sync {
    fn mark_price(&self, symbol: Symbol) -> Option<Price>;
}

/// ネットデルタ監視タスクを起動する。
///
/// `status_tx` に流した [`NetDeltaStatus`] は CSV に記録される。送信は
/// **ノンブロッキング**で、詰まっていれば捨てる（記録のために監視を止めない）。
#[allow(clippy::too_many_arguments)]
pub fn spawn_net_delta_watchdog(
    cfg: NetDeltaConfig,
    symbols: Vec<Symbol>,
    sources: Vec<Arc<dyn PositionSource>>,
    prices: Arc<dyn MarkPrices>,
    manager: Arc<Mutex<PositionManager>>,
    kill_switch: SharedKillSwitch,
    status_tx: Option<mpsc::Sender<NetDeltaStatus>>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let interval = Duration::from_secs(cfg.check_interval_secs.max(1));
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // 1 回目は即座に返るので捨てる（起動直後は板も揃っていない）。
        ticker.tick().await;

        info!(
            interval_secs = cfg.check_interval_secs,
            dexes = sources.len(),
            symbols = symbols.len(),
            "ネットデルタ監視タスクを起動"
        );

        let mut checks: u64 = 0;
        let mut skipped: u64 = 0;

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
                _ = ticker.tick() => {
                    match fetch_positions(&sources, &symbols).await {
                        Ok(positions) => {
                            checks += 1;
                            run_cycle(
                                &symbols,
                                &positions,
                                prices.as_ref(),
                                &manager,
                                &kill_switch,
                                status_tx.as_ref(),
                            );
                        }
                        Err(failed) => {
                            // 取得できなかった DEX を「フラット」と扱わない。
                            skipped += 1;
                            warn!(
                                failures = ?failed,
                                skipped_total = skipped,
                                "実ポジションを取得できない DEX があるため、この周期の\
                                 ネットデルタ判定を見送ります（フラット扱いはしない）"
                            );
                        }
                    }
                }
            }
        }
        info!(checks, skipped, "ネットデルタ監視タスク終了");
    })
}

/// 全 DEX の実ポジションを取得する。**1 つでも失敗したら `Err`。**
async fn fetch_positions(
    sources: &[Arc<dyn PositionSource>],
    symbols: &[Symbol],
) -> Result<Vec<DexPosition>, Vec<String>> {
    let mut positions = Vec::new();
    let mut failures = Vec::new();

    for source in sources {
        match source.positions(symbols).await {
            Ok(mut p) => positions.append(&mut p),
            Err(e) => failures.push(format!("{}: {e}", source.dex())),
        }
    }

    if failures.is_empty() {
        Ok(positions)
    } else {
        Err(failures)
    }
}

/// 1 周期分の判定。テストから直接呼べるよう、I/O から切り離してある。
fn run_cycle(
    symbols: &[Symbol],
    positions: &[DexPosition],
    prices: &dyn MarkPrices,
    manager: &Mutex<PositionManager>,
    kill_switch: &SharedKillSwitch,
    status_tx: Option<&mpsc::Sender<NetDeltaStatus>>,
) {
    let now = now_wall_ms();
    for symbol in symbols {
        let status = {
            let manager = lock(manager);
            manager.evaluate(*symbol, positions, prices.mark_price(*symbol), now)
        };
        report(&status);

        if let Some(reason) = status.action.halt_reason() {
            let newly_halted = {
                let mut ks = kill_switch.lock().unwrap_or_else(|e| e.into_inner());
                let was_running = ks.global_state().is_running();
                ks.halt_all(reason);
                was_running
            };
            if newly_halted {
                error!(
                    symbol = %symbol,
                    reason = reason.as_str(),
                    liquidate = reason.requires_liquidation(),
                    "ネットデルタ起因でキルスイッチを発動しました"
                );
            }
        }

        // 記録のために監視を止めない。詰まっていれば捨てる。
        if let Some(tx) = status_tx {
            if tx.try_send(status).is_err() {
                warn!(symbol = %symbol, "Net Delta CSV チャネルが詰まったため記録を破棄");
            }
        }
    }
}

fn report(status: &NetDeltaStatus) {
    if !status.action.is_noteworthy() {
        return;
    }
    warn!(
        symbol = %status.symbol,
        action = status.action.as_str(),
        net_delta = %status.net_delta,
        net_delta_usd = ?status.net_delta_usd,
        drift_usd = ?status.drift_usd,
        positions = ?status
            .positions
            .iter()
            .map(|(d, q)| format!("{d}={q}"))
            .collect::<Vec<_>>(),
        "ネットデルタが許容範囲を超えています"
    );
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // 判定中に panic しても監視は続けたいので、毒された lock も使い続ける。
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net_delta::NetDeltaAction;
    use async_trait::async_trait;
    use config::KillSwitchConfig;
    use core_types::{Dex, Quantity};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    use strategy_traits::StrategyKind;

    use crate::position::PositionError;

    struct FixedPositions {
        dex: Dex,
        quantity: Option<Decimal>,
    }

    #[async_trait]
    impl PositionSource for FixedPositions {
        fn dex(&self) -> Dex {
            self.dex
        }

        async fn positions(&self, symbols: &[Symbol]) -> Result<Vec<DexPosition>, PositionError> {
            let Some(qty) = self.quantity else {
                return Err(PositionError::Api("接続できません".into()));
            };
            Ok(symbols
                .iter()
                .map(|s| DexPosition::new(self.dex, *s, Quantity(qty)))
                .collect())
        }
    }

    struct FixedPrice(Option<Decimal>);

    impl MarkPrices for FixedPrice {
        fn mark_price(&self, _symbol: Symbol) -> Option<Price> {
            self.0.map(Price)
        }
    }

    fn source(dex: Dex, quantity: Option<Decimal>) -> Arc<dyn PositionSource> {
        Arc::new(FixedPositions { dex, quantity })
    }

    fn kill_switch() -> SharedKillSwitch {
        Arc::new(Mutex::new(KillSwitch::new(KillSwitchConfig::default())))
    }

    fn manager() -> Arc<Mutex<PositionManager>> {
        Arc::new(Mutex::new(PositionManager::new(NetDeltaConfig::default())))
    }

    #[tokio::test]
    async fn a_failing_source_aborts_the_cycle() {
        // 片方が落ちている状態で「もう片方だけ」を見ると、片肺を中立と誤認する
        let sources = vec![
            source(Dex::Hyperliquid, Some(dec!(0.5))),
            source(Dex::Lighter, None),
        ];
        let err = fetch_positions(&sources, &[Symbol::Btc]).await.unwrap_err();
        assert_eq!(err.len(), 1);
        assert!(err[0].starts_with("lighter"), "{err:?}");
    }

    #[tokio::test]
    async fn collects_positions_from_every_source() {
        let sources = vec![
            source(Dex::Hyperliquid, Some(dec!(0.5))),
            source(Dex::Lighter, Some(dec!(-0.5))),
        ];
        let positions = fetch_positions(&sources, &[Symbol::Btc]).await.unwrap();
        assert_eq!(positions.len(), 2);
    }

    #[test]
    fn critical_delta_halts_trading_and_records_the_status() {
        let ks = kill_switch();
        let manager = manager();
        let (tx, mut rx) = mpsc::channel(4);

        // 片肺 0.5 BTC（18,000 USD）→ critical（既定 1,000 USD）を超える
        run_cycle(
            &[Symbol::Btc],
            &[DexPosition::new(
                Dex::Hyperliquid,
                Symbol::Btc,
                Quantity(dec!(0.5)),
            )],
            &FixedPrice(Some(dec!(36000))),
            &manager,
            &ks,
            Some(&tx),
        );

        let status = rx.try_recv().unwrap();
        assert_eq!(status.action, NetDeltaAction::HardHalt);
        assert_eq!(status.net_delta_usd, Some(dec!(18000)));

        let ks = ks.lock().unwrap();
        assert!(!ks.can_trade(StrategyKind::FundingArb));
        assert!(!ks.can_trade(StrategyKind::PriceArb));
    }

    #[test]
    fn neutral_positions_leave_trading_running() {
        let ks = kill_switch();
        let manager = manager();
        {
            // 想定ポジションも一致している（drift ゼロ）
            let mut m = manager.lock().unwrap();
            m.expected_mut()
                .set(Dex::Hyperliquid, Symbol::Btc, Quantity(dec!(0.5)));
            m.expected_mut()
                .set(Dex::Lighter, Symbol::Btc, Quantity(dec!(-0.5)));
        }
        let (tx, mut rx) = mpsc::channel(4);

        run_cycle(
            &[Symbol::Btc],
            &[
                DexPosition::new(Dex::Hyperliquid, Symbol::Btc, Quantity(dec!(0.5))),
                DexPosition::new(Dex::Lighter, Symbol::Btc, Quantity(dec!(-0.5))),
            ],
            &FixedPrice(Some(dec!(36000))),
            &manager,
            &ks,
            Some(&tx),
        );

        assert_eq!(rx.try_recv().unwrap().action, NetDeltaAction::None);
        assert!(ks.lock().unwrap().can_trade(StrategyKind::FundingArb));
    }

    #[test]
    fn drift_soft_halts_without_requiring_liquidation() {
        let ks = kill_switch();
        let manager = manager();
        {
            // 想定は両建て。実際は Lighter 側が消えている
            let mut m = manager.lock().unwrap();
            m.expected_mut()
                .set(Dex::Hyperliquid, Symbol::Btc, Quantity(dec!(0.01)));
            m.expected_mut()
                .set(Dex::Lighter, Symbol::Btc, Quantity(dec!(-0.01)));
        }
        let (tx, mut rx) = mpsc::channel(4);

        run_cycle(
            &[Symbol::Btc],
            &[DexPosition::new(
                Dex::Hyperliquid,
                Symbol::Btc,
                Quantity(dec!(0.01)),
            )],
            &FixedPrice(Some(dec!(36000))),
            &manager,
            &ks,
            Some(&tx),
        );

        let status = rx.try_recv().unwrap();
        assert_eq!(status.action, NetDeltaAction::SoftHalt);
        let reason = ks.lock().unwrap().global_state().halt_reason().unwrap();
        assert!(!reason.requires_liquidation(), "人間の確認を待つ");
    }

    #[test]
    fn missing_price_is_reported_but_does_not_halt() {
        let ks = kill_switch();
        let manager = manager();
        let (tx, mut rx) = mpsc::channel(4);

        run_cycle(
            &[Symbol::Btc],
            &[DexPosition::new(
                Dex::Hyperliquid,
                Symbol::Btc,
                Quantity(dec!(0.5)),
            )],
            &FixedPrice(None),
            &manager,
            &ks,
            Some(&tx),
        );

        let status = rx.try_recv().unwrap();
        assert_eq!(status.action, NetDeltaAction::PriceUnavailable);
        assert!(status.action.is_noteworthy(), "気づけるように警告は出す");
        assert!(
            ks.lock().unwrap().can_trade(StrategyKind::FundingArb),
            "価格が無いだけで止めはしない（判定を保留する）"
        );
    }

    #[tokio::test]
    async fn watchdog_stops_on_shutdown() {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (status_tx, _status_rx) = mpsc::channel(4);
        let handle = spawn_net_delta_watchdog(
            NetDeltaConfig::default(),
            vec![Symbol::Btc],
            vec![source(Dex::Hyperliquid, Some(dec!(0.5)))],
            Arc::new(FixedPrice(Some(dec!(36000)))),
            manager(),
            kill_switch(),
            Some(status_tx),
            shutdown_rx,
        );

        let _ = shutdown_tx.send(true);
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("shutdown で終了しない")
            .unwrap();
    }
}
