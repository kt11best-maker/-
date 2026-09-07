//! ネットデルタ（両建てのずれ）の監視と是正判定。
//!
//! # `HedgeState` の片肺検知との違い
//!
//! 片肺検知は「**建てる瞬間**」の保護、ネットデルタ管理は「**保有し続けている
//! 間**」の保護。両者は別物で、片方だけでは不十分。
//!
//! 価格差アービトラージは保有時間が秒〜分なので前者で概ね足りるが、
//! **ファンディング裁定は数時間〜数日保有する**。その間にわずかなずれ
//! （部分約定の端数、決済時の端数、ADL、数量の丸め）が蓄積し、気づかないうちに
//! 方向性ポジションを持っている状態が最も危険である。
//!
//! ```text
//! Hyperliquid = +0.500 BTC
//! Lighter     = -0.497 BTC
//! Net Delta   = +0.003 BTC   ← 「市場中立のつもり」で持っている方向性リスク
//! ```
//!
//! # 判定はノーショナル（USD）で行う
//!
//! BTC 0.003 と HYPE 0.003 では意味が全く違う。数量ではなく **USD 換算**で
//! 閾値と比較する。価格が取れない場合は判定せず
//! （[`NetDeltaAction::PriceUnavailable`]）、**中立だと決めつけない。**
//!
//! # I/O を持たない
//!
//! ここは純粋な判定ロジック。実ポジションの取得と発注は呼び出し側
//! （[`crate::watchdog`] とフェーズ3 の執行レイヤー）の仕事。

use std::collections::BTreeMap;

use config::{NetDeltaConfig, RebalanceVenue};
use core_types::{Dex, Price, Quantity, Side, Symbol};
use rust_decimal::Decimal;

use crate::killswitch::HaltReason;
use crate::position::{DexPosition, ExpectedPositions};

/// 閾値判定の結果として取るべき行動。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetDeltaAction {
    /// 許容範囲内。
    None,
    /// ログ + 通知のみ。
    Warn,
    /// 差分だけ発注して中立に戻す。
    Rebalance,
    /// **ソフト停止**（新規発注のみ停止、人間の確認待ち）。
    SoftHalt,
    /// **ハード停止**（全ポジション解消）。
    HardHalt,
    /// 価格が取れずノーショナル換算できない。
    ///
    /// 「ずれていない」ことの確認ができていないので、**中立扱いにはしない**。
    /// 警告として扱い、価格が復旧するまで判定を保留する。
    PriceUnavailable,
}

impl NetDeltaAction {
    /// CSV の `action` 列。
    pub fn as_str(&self) -> &'static str {
        match self {
            NetDeltaAction::None => "none",
            NetDeltaAction::Warn => "warned",
            NetDeltaAction::Rebalance => "rebalanced",
            NetDeltaAction::SoftHalt => "halted_soft",
            NetDeltaAction::HardHalt => "halted",
            NetDeltaAction::PriceUnavailable => "price_unavailable",
        }
    }

    /// キルスイッチに渡す停止理由。停止を伴わない行動なら `None`。
    pub fn halt_reason(&self) -> Option<HaltReason> {
        match self {
            NetDeltaAction::SoftHalt => Some(HaltReason::PositionDrift),
            NetDeltaAction::HardHalt => Some(HaltReason::NetDeltaCritical),
            _ => None,
        }
    }

    /// ログや通知を出すべきか。
    pub fn is_noteworthy(&self) -> bool {
        !matches!(self, NetDeltaAction::None)
    }
}

/// 銘柄ごとのネットデルタ状態。
///
/// DEX ごとの値は [`BTreeMap`] で持つ（`Dex` の宣言順に並ぶため、CSV の列順が
/// 実行ごとに変わらない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetDeltaStatus {
    pub symbol: Symbol,
    /// DEX ごとの**実ポジション**（DEX API から取得した値。ロングが正）。
    pub positions: BTreeMap<Dex, Quantity>,
    /// 全 DEX 合計のネットポジション。
    pub net_delta: Quantity,
    /// ノーショナル換算のネットデルタ（USD, 絶対値）。価格が無ければ `None`。
    pub net_delta_usd: Option<Decimal>,
    /// bot が記録している想定ポジションとの差（DEX ごと）。
    pub drift_from_expected: BTreeMap<Dex, Quantity>,
    /// 想定との乖離の合計（USD, 絶対値の総和）。価格が無ければ `None`。
    pub drift_usd: Option<Decimal>,
    /// ノーショナル換算に使った価格。
    pub mark_price: Option<Price>,
    /// 閾値判定の結果。
    pub action: NetDeltaAction,
    pub checked_at_wall_ms: u64,
}

impl NetDeltaStatus {
    /// 完全に相殺できているか（数量ベース）。
    pub fn is_neutral(&self) -> bool {
        self.net_delta.raw().is_zero()
    }
}

/// リバランスの発注先候補。
///
/// 板の厚さ・手数料は `risk` の外（market-data / 設定）から渡す。この crate は
/// 板も手数料表も持たない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VenueQuote {
    pub dex: Dex,
    /// テイカー手数料（bps）。**未設定なら候補から外れる。**
    pub fee_bps: Option<Decimal>,
    /// 発注可能な板の厚さ（数量）。**未設定なら候補から外れる**
    /// （`rebalance_venue = "deeper_book"` の場合）。
    pub depth: Option<Quantity>,
}

impl VenueQuote {
    pub fn new(dex: Dex) -> Self {
        VenueQuote {
            dex,
            fee_bps: None,
            depth: None,
        }
    }

    pub fn with_fee_bps(mut self, fee_bps: Decimal) -> Self {
        self.fee_bps = Some(fee_bps);
        self
    }

    pub fn with_depth(mut self, depth: Quantity) -> Self {
        self.depth = Some(depth);
        self
    }
}

/// リバランス発注の内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebalancePlan {
    pub dex: Dex,
    pub symbol: Symbol,
    /// [`Side::Ask`] = 売り（ネットロングを削る）、[`Side::Bid`] = 買い。
    pub side: Side,
    pub quantity: Quantity,
    /// この発注のノーショナル（USD）。
    pub notional_usd: Option<Decimal>,
}

/// リバランスの判定結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebalanceDecision {
    /// 発注する。
    Order(RebalancePlan),
    /// 閾値未満。何もしない。
    NotNeeded,
    /// 価格が無く、ノーショナル判定ができない。
    PriceUnavailable,
    /// 差分が最小注文単位を下回る。
    ///
    /// **発注しない。** 無理に投げてもエラーを繰り返すだけなので警告に留める。
    BelowMinOrderQty {
        /// 是正したい数量。
        quantity: Quantity,
    },
    /// 発注先を選べない（最小注文単位が未設定 / 手数料未設定 / 板が無い）。
    NoEligibleVenue,
}

impl RebalanceDecision {
    pub fn plan(&self) -> Option<&RebalancePlan> {
        match self {
            RebalanceDecision::Order(plan) => Some(plan),
            _ => None,
        }
    }
}

/// ネットデルタの監視役（Position Manager）。
///
/// 実ポジションの取得は行わない。**渡された実ポジションを想定ポジションと
/// 突き合わせて判定するだけ**の純粋なロジック。
#[derive(Debug, Clone)]
pub struct PositionManager {
    cfg: NetDeltaConfig,
    expected: ExpectedPositions,
    consecutive_rebalance_failures: u32,
}

impl PositionManager {
    pub fn new(cfg: NetDeltaConfig) -> Self {
        PositionManager {
            cfg,
            expected: ExpectedPositions::new(),
            consecutive_rebalance_failures: 0,
        }
    }

    pub fn config(&self) -> &NetDeltaConfig {
        &self.cfg
    }

    pub fn expected(&self) -> &ExpectedPositions {
        &self.expected
    }

    pub fn expected_mut(&mut self) -> &mut ExpectedPositions {
        &mut self.expected
    }

    pub fn consecutive_rebalance_failures(&self) -> u32 {
        self.consecutive_rebalance_failures
    }

    /// 実ポジションを突き合わせて銘柄ごとの状態を作る。
    ///
    /// `actual` には**照合できた DEX の実ポジションだけ**を渡すこと。取得に
    /// 失敗した DEX を「ポジション無し」として混ぜてはいけない（片肺を
    /// 見落とす）。呼び出し側は、1 つでも取得に失敗したらその周期の判定自体を
    /// 見送る。
    ///
    /// `mark_price` はノーショナル換算に使う。取れない場合は判定せず
    /// [`NetDeltaAction::PriceUnavailable`] を返す。
    pub fn evaluate(
        &self,
        symbol: Symbol,
        actual: &[DexPosition],
        mark_price: Option<Price>,
        checked_at_wall_ms: u64,
    ) -> NetDeltaStatus {
        let mut positions: BTreeMap<Dex, Quantity> = BTreeMap::new();
        for p in actual.iter().filter(|p| p.symbol == symbol) {
            let entry = positions.entry(p.dex).or_insert(Quantity::ZERO);
            *entry = *entry + p.quantity;
        }

        let net_delta = Quantity(positions.values().map(|q| q.raw()).sum::<Decimal>());

        // 想定を持っているのに実ポジションが返ってこなかった DEX も drift の
        // 対象にする（「消えたポジション」こそ検知したい）。
        let mut drift: BTreeMap<Dex, Quantity> = BTreeMap::new();
        for dex in Dex::ALL {
            let expected = self.expected.get(dex, symbol);
            let actual_qty = positions.get(&dex).copied().unwrap_or(Quantity::ZERO);
            if positions.contains_key(&dex) || !expected.raw().is_zero() {
                drift.insert(dex, actual_qty - expected);
            }
        }

        let price = mark_price.filter(|p| p.raw() > Decimal::ZERO);
        let net_delta_usd = price.map(|p| (net_delta.raw() * p.raw()).abs());
        let drift_usd = price.map(|p| {
            drift
                .values()
                .map(|q| (q.raw() * p.raw()).abs())
                .sum::<Decimal>()
        });

        let action = self.decide(net_delta_usd, drift_usd, net_delta);

        NetDeltaStatus {
            symbol,
            positions,
            net_delta,
            net_delta_usd,
            drift_from_expected: drift,
            drift_usd,
            mark_price: price,
            action,
            checked_at_wall_ms,
        }
    }

    /// 閾値判定。**重い順に評価する。**
    fn decide(
        &self,
        net_delta_usd: Option<Decimal>,
        drift_usd: Option<Decimal>,
        net_delta: Quantity,
    ) -> NetDeltaAction {
        let (Some(net_usd), Some(drift_usd)) = (net_delta_usd, drift_usd) else {
            // 価格が無い。ずれていないと確認できたわけではないので、
            // 完全に相殺しているとき以外は「不明」として扱う。
            return if net_delta.raw().is_zero() {
                NetDeltaAction::None
            } else {
                NetDeltaAction::PriceUnavailable
            };
        };

        if net_usd > self.cfg.critical_threshold_usd {
            return NetDeltaAction::HardHalt;
        }
        // 自分のポジションを把握できていない状態での発注は危険なので、
        // リバランス判定より先にここで止める。
        if drift_usd > self.cfg.max_drift_usd {
            return NetDeltaAction::SoftHalt;
        }
        if net_usd > self.cfg.rebalance_threshold_usd {
            return NetDeltaAction::Rebalance;
        }
        if net_usd > self.cfg.warn_threshold_usd {
            return NetDeltaAction::Warn;
        }
        NetDeltaAction::None
    }

    /// 是正のための発注内容を決める。
    ///
    /// ネットロング（`net_delta > 0`）なら**売り**、ネットショートなら**買い**を
    /// 差分だけ出す。発注先は設定（[`RebalanceVenue`]）に従い、板が厚い方または
    /// 手数料が安い方を選ぶ。
    ///
    /// 最小注文単位が未設定の DEX は候補から外す（手数料と同じ扱い。確認前の値で
    /// 発注させないため）。差分が最小注文単位を下回る場合は
    /// [`RebalanceDecision::BelowMinOrderQty`] を返し、**発注しない**。
    pub fn plan_rebalance(
        &self,
        status: &NetDeltaStatus,
        venues: &[VenueQuote],
    ) -> RebalanceDecision {
        if status.action == NetDeltaAction::PriceUnavailable {
            return RebalanceDecision::PriceUnavailable;
        }
        if status.action != NetDeltaAction::Rebalance {
            return RebalanceDecision::NotNeeded;
        }

        let quantity = Quantity(status.net_delta.raw().abs());
        if quantity.raw().is_zero() {
            return RebalanceDecision::NotNeeded;
        }

        // 最小注文単位が分かっていて、かつ差分がそれ以上ある DEX だけが候補。
        let eligible: Vec<&VenueQuote> = venues
            .iter()
            .filter(|v| {
                self.cfg
                    .min_order_qty(v.dex, status.symbol)
                    .is_some_and(|min| quantity.raw() >= min)
            })
            .collect();

        if eligible.is_empty() {
            // 「単位が分かっていて足りない」のか「そもそも分からない」のかを
            // 区別する。前者は待てば解消しうるが、後者は設定漏れ。
            let known_min = venues
                .iter()
                .any(|v| self.cfg.min_order_qty(v.dex, status.symbol).is_some());
            return if known_min {
                RebalanceDecision::BelowMinOrderQty { quantity }
            } else {
                RebalanceDecision::NoEligibleVenue
            };
        }

        let Some(venue) = self.select_venue(&eligible) else {
            return RebalanceDecision::NoEligibleVenue;
        };

        // ネットロングなら売って削る。ネットショートなら買って埋める。
        let side = if status.net_delta.raw() > Decimal::ZERO {
            Side::Ask
        } else {
            Side::Bid
        };

        RebalanceDecision::Order(RebalancePlan {
            dex: venue.dex,
            symbol: status.symbol,
            side,
            quantity,
            notional_usd: status.mark_price.map(|p| quantity.raw() * p.raw()),
        })
    }

    /// 設定に従って発注先を 1 つ選ぶ。同値なら `Dex` の宣言順（決定的）。
    fn select_venue<'a>(&self, eligible: &[&'a VenueQuote]) -> Option<&'a VenueQuote> {
        match self.cfg.rebalance_venue {
            RebalanceVenue::CheaperFee => eligible
                .iter()
                .filter(|v| v.fee_bps.is_some())
                .min_by(|a, b| {
                    a.fee_bps
                        .unwrap()
                        .cmp(&b.fee_bps.unwrap())
                        .then(a.dex.cmp(&b.dex))
                })
                .copied(),
            RebalanceVenue::DeeperBook => eligible
                .iter()
                .filter(|v| v.depth.is_some())
                .max_by(|a, b| {
                    a.depth
                        .unwrap()
                        .cmp(&b.depth.unwrap())
                        .then(b.dex.cmp(&a.dex))
                })
                .copied(),
        }
    }

    /// リバランス発注の結果を記録する。
    ///
    /// 規定回数連続で失敗したらソフト停止させる（是正できないまま発注し続けても
    /// 手数料を払うだけ）。戻り値は、このタイミングで**新たに**required になった
    /// 停止理由。
    pub fn record_rebalance_outcome(&mut self, succeeded: bool) -> Option<HaltReason> {
        if succeeded {
            self.consecutive_rebalance_failures = 0;
            return None;
        }
        self.consecutive_rebalance_failures += 1;
        (self.consecutive_rebalance_failures == self.cfg.max_consecutive_rebalance_failures)
            .then_some(HaltReason::RebalanceFailed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    const NOW: u64 = 1_700_000_000_000;
    /// BTC 36,000 USD。0.003 BTC ≒ 108 USD。
    const BTC: Price = Price(Decimal::from_parts(36_000, 0, 0, false, 0));

    fn config() -> NetDeltaConfig {
        let mut cfg = NetDeltaConfig {
            warn_threshold_usd: dec!(50),
            rebalance_threshold_usd: dec!(200),
            critical_threshold_usd: dec!(1000),
            max_drift_usd: dec!(100),
            ..NetDeltaConfig::default()
        };
        for dex in [Dex::Hyperliquid, Dex::Lighter] {
            cfg.min_order_qty
                .entry(dex)
                .or_default()
                .insert(Symbol::Btc, dec!(0.0001));
        }
        cfg
    }

    fn manager() -> PositionManager {
        PositionManager::new(config())
    }

    fn pos(dex: Dex, qty: Decimal) -> DexPosition {
        DexPosition::new(dex, Symbol::Btc, Quantity(qty))
    }

    fn evaluate(m: &PositionManager, actual: &[DexPosition]) -> NetDeltaStatus {
        m.evaluate(Symbol::Btc, actual, Some(BTC), NOW)
    }

    /// 実ポジションと**同じ想定**を持つ manager。
    ///
    /// drift をゼロにして、ネットデルタ側の閾値判定だけを見たいときに使う
    /// （drift 超過はネットデルタの是正判定より優先されるため）。
    fn expecting(actual: &[DexPosition]) -> PositionManager {
        let mut m = manager();
        for p in actual {
            m.expected_mut().set(p.dex, p.symbol, p.quantity);
        }
        m
    }

    #[test]
    fn perfectly_hedged_positions_are_neutral() {
        let actual = [
            pos(Dex::Hyperliquid, dec!(0.5)),
            pos(Dex::Lighter, dec!(-0.5)),
        ];
        let status = evaluate(&expecting(&actual), &actual);
        assert_eq!(status.net_delta, Quantity::ZERO);
        assert_eq!(status.net_delta_usd, Some(Decimal::ZERO));
        assert!(status.is_neutral());
        assert_eq!(status.action, NetDeltaAction::None);
    }

    #[test]
    fn shortfall_on_one_side_is_signed() {
        // Hyperliquid +0.500 / Lighter -0.497 → net +0.003（ネットロング）
        let actual = [
            pos(Dex::Hyperliquid, dec!(0.5)),
            pos(Dex::Lighter, dec!(-0.497)),
        ];
        let status = evaluate(&expecting(&actual), &actual);
        assert_eq!(status.net_delta, Quantity(dec!(0.003)));
        assert_eq!(status.positions[&Dex::Hyperliquid], Quantity(dec!(0.5)));
        assert_eq!(status.positions[&Dex::Lighter], Quantity(dec!(-0.497)));

        // 逆向きなら符号も逆
        let flipped_actual = [
            pos(Dex::Hyperliquid, dec!(0.497)),
            pos(Dex::Lighter, dec!(-0.5)),
        ];
        let flipped = evaluate(&expecting(&flipped_actual), &flipped_actual);
        assert_eq!(flipped.net_delta, Quantity(dec!(-0.003)));
    }

    #[test]
    fn notional_conversion_multiplies_by_price() {
        // 0.003 BTC × 36,000 = 108 USD
        let actual = [
            pos(Dex::Hyperliquid, dec!(0.5)),
            pos(Dex::Lighter, dec!(-0.497)),
        ];
        let m = expecting(&actual);
        let status = evaluate(&m, &actual);
        assert_eq!(status.net_delta_usd, Some(dec!(108.000)));
        // 数量が同じでも価格が違えば意味が違う（だから USD で判定する）
        let cheap = m.evaluate(Symbol::Btc, &actual, Some(Price(dec!(30))), NOW);
        assert_eq!(cheap.net_delta_usd, Some(dec!(0.090)));
        assert_eq!(cheap.action, NetDeltaAction::None, "USD では小さい");
    }

    #[test]
    fn thresholds_select_the_expected_action() {
        // 想定どおりのポジションでも、両建てが崩れていれば検知する
        // （drift ではなくネットデルタ側の判定を見るため想定を一致させる）
        let action_for = |qty: Decimal| {
            let actual = [pos(Dex::Hyperliquid, qty)];
            evaluate(&expecting(&actual), &actual).action
        };

        // 36 USD → 何もしない
        assert_eq!(action_for(dec!(0.001)), NetDeltaAction::None);
        // 108 USD → 警告のみ
        assert_eq!(action_for(dec!(0.003)), NetDeltaAction::Warn);
        // 360 USD → リバランス
        assert_eq!(action_for(dec!(0.01)), NetDeltaAction::Rebalance);
        // 3,600 USD → ハード停止（全ポジション解消）
        assert_eq!(action_for(dec!(0.1)), NetDeltaAction::HardHalt);

        let critical = {
            let actual = [pos(Dex::Hyperliquid, dec!(0.1))];
            evaluate(&expecting(&actual), &actual)
        };
        assert_eq!(
            critical.action.halt_reason(),
            Some(HaltReason::NetDeltaCritical)
        );
        assert!(critical
            .action
            .halt_reason()
            .unwrap()
            .requires_liquidation());
    }

    #[test]
    fn drift_beyond_the_limit_soft_halts() {
        let mut m = manager();
        // 想定は完全な両建て
        m.expected_mut()
            .set(Dex::Hyperliquid, Symbol::Btc, Quantity(dec!(0.5)));
        m.expected_mut()
            .set(Dex::Lighter, Symbol::Btc, Quantity(dec!(-0.5)));

        // 実際は Lighter が 0.01 少ない（= 360 USD の乖離）
        let status = evaluate(
            &m,
            &[
                pos(Dex::Hyperliquid, dec!(0.5)),
                pos(Dex::Lighter, dec!(-0.49)),
            ],
        );
        assert_eq!(
            status.drift_from_expected[&Dex::Lighter],
            Quantity(dec!(0.01))
        );
        assert_eq!(status.drift_usd, Some(dec!(360.00)));
        assert_eq!(status.action, NetDeltaAction::SoftHalt);

        let reason = status.action.halt_reason().unwrap();
        assert_eq!(reason, HaltReason::PositionDrift);
        assert!(
            !reason.requires_liquidation(),
            "把握できていない状態で自動解消に動かない"
        );
    }

    #[test]
    fn drift_is_detected_even_when_the_expectation_looks_fine() {
        // **想定値だけ見ていたら気づけないケース。**
        // 想定は +0.5 / -0.5 で完全中立。実ポジションも合計はほぼ中立だが、
        // 両側とも想定からずれている（片方が ADL で削られ、片方が過剰約定）。
        let mut m = manager();
        m.expected_mut()
            .set(Dex::Hyperliquid, Symbol::Btc, Quantity(dec!(0.5)));
        m.expected_mut()
            .set(Dex::Lighter, Symbol::Btc, Quantity(dec!(-0.5)));
        assert_eq!(
            m.expected().net(Symbol::Btc),
            Quantity::ZERO,
            "想定だけ見ると完全中立"
        );

        let status = evaluate(
            &m,
            &[
                pos(Dex::Hyperliquid, dec!(0.4)),
                pos(Dex::Lighter, dec!(-0.4)),
            ],
        );
        assert!(status.is_neutral(), "ネットデルタはゼロ");
        assert_eq!(status.net_delta_usd, Some(Decimal::ZERO));
        // それでも drift は検知される（0.1 × 2 × 36,000 = 7,200 USD）
        assert_eq!(status.drift_usd, Some(dec!(7200.00)));
        assert_eq!(status.action, NetDeltaAction::SoftHalt);
    }

    #[test]
    fn missing_position_for_an_expected_dex_counts_as_drift() {
        // 片方の DEX がポジションを返さない（= 強制決済の疑い）
        let mut m = manager();
        m.expected_mut()
            .set(Dex::Hyperliquid, Symbol::Btc, Quantity(dec!(0.01)));
        m.expected_mut()
            .set(Dex::Lighter, Symbol::Btc, Quantity(dec!(-0.01)));

        let status = evaluate(&m, &[pos(Dex::Hyperliquid, dec!(0.01))]);
        assert_eq!(
            status.drift_from_expected[&Dex::Lighter],
            Quantity(dec!(0.01)),
            "消えたポジションも drift として数える"
        );
        assert_eq!(status.action, NetDeltaAction::SoftHalt);
    }

    #[test]
    fn critical_delta_outranks_drift() {
        let mut m = manager();
        m.expected_mut()
            .set(Dex::Lighter, Symbol::Btc, Quantity(dec!(-0.5)));
        // drift も critical も超えている場合は重い方（ハード停止）
        let status = evaluate(&m, &[pos(Dex::Hyperliquid, dec!(0.5))]);
        assert_eq!(status.action, NetDeltaAction::HardHalt);
    }

    #[test]
    fn missing_price_never_reads_as_neutral() {
        let m = expecting(&[
            pos(Dex::Hyperliquid, dec!(0.5)),
            pos(Dex::Lighter, dec!(-0.5)),
        ]);
        let unknown = m.evaluate(Symbol::Btc, &[pos(Dex::Hyperliquid, dec!(0.5))], None, NOW);
        assert_eq!(unknown.action, NetDeltaAction::PriceUnavailable);
        assert_eq!(unknown.net_delta_usd, None);

        // 価格が 0 でも同じ扱い（ゼロ除算・ゼロ換算で中立に見せない）
        let zero_price = m.evaluate(
            Symbol::Btc,
            &[pos(Dex::Hyperliquid, dec!(0.5))],
            Some(Price::ZERO),
            NOW,
        );
        assert_eq!(zero_price.action, NetDeltaAction::PriceUnavailable);

        // ただし完全に相殺していれば価格が無くても問題ない
        let hedged = m.evaluate(
            Symbol::Btc,
            &[
                pos(Dex::Hyperliquid, dec!(0.5)),
                pos(Dex::Lighter, dec!(-0.5)),
            ],
            None,
            NOW,
        );
        assert_eq!(hedged.action, NetDeltaAction::None);
    }

    #[test]
    fn rebalance_orders_the_difference_on_the_cheaper_venue() {
        // net +0.01 BTC（360 USD）→ 売って削る
        let actual = [
            pos(Dex::Hyperliquid, dec!(0.5)),
            pos(Dex::Lighter, dec!(-0.49)),
        ];
        let m = expecting(&actual);
        let status = evaluate(&m, &actual);
        assert_eq!(status.action, NetDeltaAction::Rebalance);

        let venues = [
            VenueQuote::new(Dex::Hyperliquid).with_fee_bps(dec!(4.5)),
            VenueQuote::new(Dex::Lighter).with_fee_bps(dec!(2.0)),
        ];
        let plan = m.plan_rebalance(&status, &venues).plan().copied().unwrap();
        assert_eq!(plan.dex, Dex::Lighter, "手数料が安い方");
        assert_eq!(plan.side, Side::Ask, "ネットロングは売って削る");
        assert_eq!(plan.quantity, Quantity(dec!(0.01)));
        assert_eq!(plan.notional_usd, Some(dec!(360.00)));
    }

    #[test]
    fn net_short_is_corrected_by_buying() {
        let actual = [
            pos(Dex::Hyperliquid, dec!(0.49)),
            pos(Dex::Lighter, dec!(-0.5)),
        ];
        let m = expecting(&actual);
        let status = evaluate(&m, &actual);
        let venues = [VenueQuote::new(Dex::Hyperliquid).with_fee_bps(dec!(4.5))];
        let plan = m.plan_rebalance(&status, &venues).plan().copied().unwrap();
        assert_eq!(plan.side, Side::Bid);
        assert_eq!(plan.quantity, Quantity(dec!(0.01)));
    }

    #[test]
    fn deeper_book_policy_picks_the_thicker_venue() {
        let mut cfg = config();
        cfg.rebalance_venue = RebalanceVenue::DeeperBook;
        let mut m = PositionManager::new(cfg);
        m.expected_mut()
            .set(Dex::Hyperliquid, Symbol::Btc, Quantity(dec!(0.01)));
        let status = evaluate(&m, &[pos(Dex::Hyperliquid, dec!(0.01))]);

        let venues = [
            VenueQuote::new(Dex::Hyperliquid)
                .with_fee_bps(dec!(1))
                .with_depth(Quantity(dec!(2))),
            VenueQuote::new(Dex::Lighter)
                .with_fee_bps(dec!(9))
                .with_depth(Quantity(dec!(5))),
        ];
        let plan = m.plan_rebalance(&status, &venues).plan().copied().unwrap();
        assert_eq!(plan.dex, Dex::Lighter, "板が厚い方（手数料は見ない）");
    }

    #[test]
    fn difference_below_min_order_qty_is_not_ordered() {
        // 是正したくてもできない差分では発注しない（エラーを繰り返すだけ）
        let mut cfg = config();
        cfg.rebalance_threshold_usd = dec!(1);
        cfg.warn_threshold_usd = Decimal::new(5, 1);
        cfg.critical_threshold_usd = dec!(100_000);
        for dex in [Dex::Hyperliquid, Dex::Lighter] {
            cfg.min_order_qty
                .entry(dex)
                .or_default()
                .insert(Symbol::Btc, dec!(0.01));
        }
        let mut m = PositionManager::new(cfg);
        m.expected_mut()
            .set(Dex::Hyperliquid, Symbol::Btc, Quantity(dec!(0.001)));

        // 0.001 BTC = 36 USD はリバランス閾値を超えるが、最小注文単位に満たない
        let status = evaluate(&m, &[pos(Dex::Hyperliquid, dec!(0.001))]);
        assert_eq!(status.action, NetDeltaAction::Rebalance);

        let venues = [
            VenueQuote::new(Dex::Hyperliquid).with_fee_bps(dec!(4.5)),
            VenueQuote::new(Dex::Lighter).with_fee_bps(dec!(2)),
        ];
        assert_eq!(
            m.plan_rebalance(&status, &venues),
            RebalanceDecision::BelowMinOrderQty {
                quantity: Quantity(dec!(0.001))
            }
        );
    }

    #[test]
    fn venue_without_configured_min_qty_is_not_used() {
        let m = expecting(&[pos(Dex::Hyperliquid, dec!(0.01))]);
        let status = evaluate(&m, &[pos(Dex::Hyperliquid, dec!(0.01))]);
        // 最小注文単位が未設定の DEX しか候補が無い（手数料と同じく推測しない）
        let venues = [VenueQuote::new(Dex::EdgeX).with_fee_bps(dec!(1))];
        assert_eq!(
            m.plan_rebalance(&status, &venues),
            RebalanceDecision::NoEligibleVenue
        );

        // 手数料が未設定なら cheaper_fee では選べない
        let venues = [VenueQuote::new(Dex::Lighter)];
        assert_eq!(
            m.plan_rebalance(&status, &venues),
            RebalanceDecision::NoEligibleVenue
        );
    }

    #[test]
    fn no_rebalance_below_the_threshold() {
        let m = expecting(&[pos(Dex::Hyperliquid, dec!(0.003))]);
        let status = evaluate(&m, &[pos(Dex::Hyperliquid, dec!(0.003))]);
        assert_eq!(status.action, NetDeltaAction::Warn);
        let venues = [VenueQuote::new(Dex::Lighter).with_fee_bps(dec!(2))];
        assert_eq!(
            m.plan_rebalance(&status, &venues),
            RebalanceDecision::NotNeeded
        );
    }

    #[test]
    fn repeated_rebalance_failures_soft_halt() {
        let mut m = manager();
        assert_eq!(m.record_rebalance_outcome(false), None);
        assert_eq!(m.record_rebalance_outcome(false), None);
        assert_eq!(
            m.record_rebalance_outcome(false),
            Some(HaltReason::RebalanceFailed),
            "既定 3 回連続でソフト停止"
        );
        assert!(!HaltReason::RebalanceFailed.requires_liquidation());

        // 成功したら数え直し
        m.record_rebalance_outcome(true);
        assert_eq!(m.consecutive_rebalance_failures(), 0);
        assert_eq!(m.record_rebalance_outcome(false), None);
    }

    #[test]
    fn action_labels_match_the_csv_contract() {
        assert_eq!(NetDeltaAction::None.as_str(), "none");
        assert_eq!(NetDeltaAction::Warn.as_str(), "warned");
        assert_eq!(NetDeltaAction::Rebalance.as_str(), "rebalanced");
        assert_eq!(NetDeltaAction::SoftHalt.as_str(), "halted_soft");
        assert_eq!(NetDeltaAction::HardHalt.as_str(), "halted");
        assert!(!NetDeltaAction::None.is_noteworthy());
        assert!(NetDeltaAction::PriceUnavailable.is_noteworthy());
    }
}
