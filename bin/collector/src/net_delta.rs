//! ネットデルタ監視の結線。
//!
//! # フェーズ1 では動かない（構造だけ用意する）
//!
//! ネットデルタ監視は**各 DEX の実ポジションを API から取得**して初めて意味を
//! 持つ。認証付き API と発注機能はフェーズ3 なので、フェーズ1 の collector には
//! [`PositionSource`] の実装が 1 つも無い。
//!
//! そのため起動時にその旨を報告し、監視タスクは起動しない。**結線だけは
//! 済ませてある**ので、フェーズ3 で各 DEX の実装を [`build_position_sources`] に
//! 足せばそのまま動く。

use std::sync::{Arc, Mutex};

use config::Config;
use core_types::{Dex, Price, Symbol};
use market_data::BookStore;
use risk::{
    spawn_net_delta_watchdog, MarkPrices, PositionManager, PositionSource, SharedKillSwitch,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// ノーショナル換算に使う価格を板から取る。
///
/// **一番新しい板の mid を使う。** DEX 間で価格が乖離していても、閾値判定
/// （USD 数十〜数千のオーダー）に効くほどの差にはならないため、平均や中央値まで
/// 凝る必要はない。板が 1 つも無ければ `None` を返す（**0 を返してはいけない**。
/// ずれを中立だと誤認する）。
pub struct BookMidPrices {
    store: Arc<BookStore>,
    dexes: Vec<Dex>,
}

impl BookMidPrices {
    pub fn new(store: Arc<BookStore>, dexes: Vec<Dex>) -> Self {
        BookMidPrices { store, dexes }
    }
}

impl MarkPrices for BookMidPrices {
    fn mark_price(&self, symbol: Symbol) -> Option<Price> {
        self.dexes
            .iter()
            .filter_map(|dex| self.store.get(*dex, symbol))
            // クロスした板（dYdX では正常に起こる）の mid は信用しない
            .filter(|book| !book.is_crossed())
            .max_by_key(|book| book.trace.received_wall_ms)
            .and_then(|book| book.mid())
    }
}

/// 実ポジション取得の実装を組み立てる。
///
/// **フェーズ1 では空。** フェーズ3 で各 DEX の実装（認証付き REST / WS）を
/// ここに足す。
fn build_position_sources(_cfg: &Config) -> Vec<Arc<dyn PositionSource>> {
    Vec::new()
}

/// ネットデルタ監視と、その CSV 記録を起動する。
///
/// 実ポジションを取得できる DEX が 1 つも無い場合は**起動しない**
/// （取れない DEX を「フラット」と誤認しないため）。その旨はログに残す。
pub fn spawn_net_delta_monitoring(
    cfg: &Config,
    store: Arc<BookStore>,
    kill_switch: SharedKillSwitch,
    shutdown: watch::Receiver<bool>,
) -> Vec<JoinHandle<()>> {
    if !cfg.risk.net_delta.enabled {
        info!("risk.net_delta.enabled = false のためネットデルタ監視は起動しません");
        return Vec::new();
    }

    let sources = build_position_sources(cfg);
    if sources.is_empty() {
        warn!(
            "実ポジション取得 API（PositionSource）の実装がまだ無いため、ネットデルタ監視は\
             起動しません。フェーズ3 で発注機能と併せて有効化してください\
             （判定ロジックと CSV 記録は実装済みで、テストも回っています）"
        );
        return Vec::new();
    }

    let symbols = cfg.general.symbols.clone();
    let missing = cfg
        .risk
        .net_delta
        .dexes_missing_min_order_qty(&cfg.enabled_dexes(), &symbols);
    for dex in missing {
        // 手数料と同じ扱い。確認前の値で発注させない
        warn!(
            dex = %dex,
            "risk.net_delta.min_order_qty が未設定です。この DEX はリバランスの発注先に\
             選ばれません（監視と記録は行われます）"
        );
    }

    let (status_tx, status_rx) = mpsc::channel(cfg.recording.snapshot_channel_capacity.min(256));
    let csv_handle = recorder::spawn_net_delta_csv_writer(&cfg.recording, status_rx);

    let prices = Arc::new(BookMidPrices::new(store, cfg.enabled_dexes()));
    let manager = Arc::new(Mutex::new(PositionManager::new(cfg.risk.net_delta.clone())));
    let watchdog_handle = spawn_net_delta_watchdog(
        cfg.risk.net_delta.clone(),
        symbols,
        sources,
        prices,
        manager,
        kill_switch,
        Some(status_tx),
        shutdown,
    );

    vec![watchdog_handle, csv_handle]
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{Level, MessageTrace, OrderBook, Quantity};
    use rust_decimal_macros::dec;

    fn book(
        dex: Dex,
        bid: rust_decimal::Decimal,
        ask: rust_decimal::Decimal,
        received_ms: u64,
    ) -> OrderBook {
        let mut trace = MessageTrace::on_receive();
        trace.received_wall_ms = received_ms;
        OrderBook::new(
            dex,
            Symbol::Btc,
            vec![Level::new(Price(bid), Quantity(dec!(1)))],
            vec![Level::new(Price(ask), Quantity(dec!(1)))],
            trace,
        )
    }

    #[test]
    fn uses_the_freshest_book() {
        let store = Arc::new(BookStore::new());
        store.update(book(Dex::Hyperliquid, dec!(36000), dec!(36002), 1_000));
        store.update(book(Dex::Lighter, dec!(35000), dec!(35002), 2_000));

        let prices = BookMidPrices::new(Arc::clone(&store), vec![Dex::Hyperliquid, Dex::Lighter]);
        assert_eq!(prices.mark_price(Symbol::Btc), Some(Price(dec!(35001))));
    }

    #[test]
    fn no_book_yields_no_price() {
        let store = Arc::new(BookStore::new());
        let prices = BookMidPrices::new(Arc::clone(&store), vec![Dex::Hyperliquid]);
        // **0 ではなく None。** ずれを中立だと誤認しないため
        assert_eq!(prices.mark_price(Symbol::Btc), None);

        // クロスした板も使わない（dYdX では構造上正常に起こる）
        store.update(book(Dex::Dydx, dec!(36002), dec!(36000), 1_000));
        let prices = BookMidPrices::new(store, vec![Dex::Dydx]);
        assert_eq!(prices.mark_price(Symbol::Btc), None);
    }
}
