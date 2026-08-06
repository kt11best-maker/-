//! TOML 設定ファイルの読み込み。
//!
//! 閾値類はすべてここを経由し、コード変更なしで調整できるようにする。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use core_types::{Dex, Symbol};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("設定ファイルを読めません ({path}): {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("設定ファイルの構文エラー ({path}): {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("設定値が不正: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub dex: DexConfig,
    #[serde(default)]
    pub recording: RecordingConfig,
    #[serde(default)]
    pub monitoring: MonitoringConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GeneralConfig {
    /// 監視対象銘柄。
    #[serde(default = "default_symbols")]
    pub symbols: Vec<Symbol>,
    /// 取得する板のレベル数。
    #[serde(default = "default_depth")]
    pub orderbook_depth: usize,
    /// VWAP ベース乖離を計算する際の想定ノーショナル（USD）。
    /// 0 以下にすると VWAP 乖離の計算を行わない。
    #[serde(default = "default_vwap_notional")]
    pub vwap_notional_usd: Decimal,
    /// 板受信チャネルの容量。溢れた場合は受信側で待たされるため、
    /// バースト時のバッファとして十分な値にしておく。
    #[serde(default = "default_book_channel_capacity")]
    pub book_channel_capacity: usize,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DexConfig {
    #[serde(default)]
    pub hyperliquid: HyperliquidConfig,
    #[serde(default)]
    pub edgex: EdgeXConfig,
    #[serde(default)]
    pub aster: AsterConfig,
    #[serde(default)]
    pub lighter: LighterConfig,
}

/// 再接続まわりの共通パラメータ。
#[derive(Debug, Clone, Copy)]
pub struct ReconnectPolicy {
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HyperliquidConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_hl_ws_url")]
    pub ws_url: String,
    #[serde(default = "default_reconnect_max_attempts")]
    pub reconnect_max_attempts: u32,
    #[serde(default = "default_reconnect_base_delay_ms")]
    pub reconnect_base_delay_ms: u64,
    #[serde(default = "default_reconnect_max_delay_ms")]
    pub reconnect_max_delay_ms: u64,
    /// keepalive の ping 間隔（秒）。Hyperliquid は無通信が続くと切断するため必須。
    #[serde(default = "default_hl_ping_interval_secs")]
    pub ping_interval_secs: u64,
    /// この DEX では購読しない銘柄（市場が存在しない場合など）。
    #[serde(default)]
    pub excluded_symbols: Vec<Symbol>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeXConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_edgex_ws_url")]
    pub ws_url: String,
    #[serde(default = "default_reconnect_max_attempts")]
    pub reconnect_max_attempts: u32,
    #[serde(default = "default_reconnect_base_delay_ms")]
    pub reconnect_base_delay_ms: u64,
    #[serde(default = "default_reconnect_max_delay_ms")]
    pub reconnect_max_delay_ms: u64,
    #[serde(default = "default_edgex_ping_interval_secs")]
    pub ping_interval_secs: u64,
    /// depth チャネルが受け付けるレベル数（API 側で離散値に制限される）。
    #[serde(default = "default_edgex_depth_level")]
    pub depth_level: usize,
    /// 差分再構築のズレを検出するため、定期的に再購読して全量スナップショットを
    /// 取り直す間隔（秒）。0 で無効。
    #[serde(default = "default_edgex_resync_interval_secs")]
    pub resync_interval_secs: u64,
    /// 起動時に REST メタデータから contractId を解決するか。
    /// false の場合は `contract_ids` の値をそのまま使う。
    #[serde(default = "default_true")]
    pub resolve_contract_ids: bool,
    /// contractId 解決に使う REST エンドポイント。
    #[serde(default = "default_edgex_metadata_url")]
    pub metadata_url: String,
    /// 銘柄 → contractId の手動マッピング（解決失敗時のフォールバック / 上書き）。
    /// 例: `BTC = "10000001"`
    #[serde(default)]
    pub contract_ids: BTreeMap<Symbol, String>,
    /// この DEX では購読しない銘柄。
    #[serde(default)]
    pub excluded_symbols: Vec<Symbol>,
}

/// Aster（Binance 系 API）の設定。
///
/// 差分更新方式のため、板の基準となるスナップショットは REST でしか取れない。
/// `pu` の連続性が崩れるたびに REST を叩き直すので、`resync_backoff_ms` を必ず
/// 効かせて IP ban を避けること。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AsterConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_aster_ws_url")]
    pub ws_url: String,
    /// Futures/Perp 用の REST ベース URL。Spot 用の `sapi` ではなく `fapi` を使う。
    #[serde(default = "default_aster_rest_url")]
    pub rest_url: String,
    /// depth ストリームの更新間隔。空文字なら `<symbol>@depth`（既定速度）。
    #[serde(default = "default_aster_depth_interval")]
    pub depth_stream_interval: String,
    /// REST スナップショットで取得する板のレベル数。
    #[serde(default = "default_aster_snapshot_limit")]
    pub snapshot_limit: u32,
    /// 24 時間の強制切断を待たずに能動的に張り直す間隔（時間）。0 で無効。
    #[serde(default = "default_aster_reconnect_before_hours")]
    pub reconnect_before_hours: u64,
    #[serde(default = "default_reconnect_max_attempts")]
    pub reconnect_max_attempts: u32,
    #[serde(default = "default_reconnect_base_delay_ms")]
    pub reconnect_base_delay_ms: u64,
    #[serde(default = "default_reconnect_max_delay_ms")]
    pub reconnect_max_delay_ms: u64,
    /// 板再初期化（REST スナップショット再取得）の最小間隔。IP ban 回避用。
    #[serde(default = "default_aster_resync_backoff_ms")]
    pub resync_backoff_ms: u64,
    /// この DEX では購読しない銘柄。
    #[serde(default)]
    pub excluded_symbols: Vec<Symbol>,
}

/// Lighter の設定。
///
/// フェーズ1 では REST を一切使わない。板もシンボルマッピングも WS で完結する。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LighterConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_lighter_ws_url")]
    pub ws_url: String,
    #[serde(default = "default_lighter_testnet_ws_url")]
    pub testnet_ws_url: String,
    #[serde(default)]
    pub use_testnet: bool,
    /// クライアント側 keepalive の送信間隔（秒）。
    /// Lighter は 2 分間フレームを送らないとサーバーが接続を閉じる。
    #[serde(default = "default_lighter_keepalive_secs")]
    pub keepalive_interval_secs: u64,
    #[serde(default = "default_reconnect_max_attempts")]
    pub reconnect_max_attempts: u32,
    #[serde(default = "default_reconnect_base_delay_ms")]
    pub reconnect_base_delay_ms: u64,
    #[serde(default = "default_reconnect_max_delay_ms")]
    pub reconnect_max_delay_ms: u64,
    /// market_stats から market_index を解決できないまま経過した場合に
    /// 警告を出すまでの時間（秒）。
    #[serde(default = "default_lighter_mapping_warn_secs")]
    pub mapping_warn_secs: u64,
    /// この DEX では購読しない銘柄。
    #[serde(default)]
    pub excluded_symbols: Vec<Symbol>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CsvMode {
    /// 全更新を記録する（フェーズ1の初期設定）。
    All,
    /// 銘柄ごとに `csv_sample_interval_ms` 間隔で間引く。
    Sampled,
    /// 乖離が `csv_threshold_bps` を超えたスナップショットのみ記録する。
    Threshold,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingConfig {
    #[serde(default = "default_csv_dir")]
    pub csv_dir: PathBuf,
    #[serde(default = "default_log_dir")]
    pub log_dir: PathBuf,
    #[serde(default = "default_csv_mode")]
    pub csv_mode: CsvMode,
    #[serde(default = "default_csv_sample_interval_ms")]
    pub csv_sample_interval_ms: u64,
    #[serde(default = "default_csv_threshold_bps")]
    pub csv_threshold_bps: Decimal,
    /// CSV バッファを flush する間隔（ms）。
    #[serde(default = "default_csv_flush_interval_ms")]
    pub csv_flush_interval_ms: u64,
    /// 価格差スナップショット送信チャネルの容量。
    #[serde(default = "default_snapshot_channel_capacity")]
    pub snapshot_channel_capacity: usize,
    /// ログレベル（`RUST_LOG` 相当の文字列）。
    #[serde(default = "default_log_filter")]
    pub log_filter: String,
    /// ログを標準出力にも出すか。
    #[serde(default)]
    pub log_to_stdout: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MonitoringConfig {
    /// 取引所 → 受信の遅延がこれを超えたら警告ログを出す。
    #[serde(default = "default_latency_warn_ms")]
    pub latency_warn_threshold_ms: i64,
    /// 両 DEX の鮮度差がこれを超えたら警告ログを出す。
    #[serde(default = "default_staleness_warn_ms")]
    pub staleness_warn_threshold_ms: i64,
    /// 接続状態・スループットを定期出力する間隔（秒）。
    #[serde(default = "default_status_interval_secs")]
    pub status_interval_secs: u64,
    /// 同種の警告を出す最小間隔（秒）。ログ肥大を防ぐためのレート制限。
    #[serde(default = "default_warn_min_interval_secs")]
    pub warn_min_interval_secs: u64,
}

impl Config {
    /// TOML ファイルを読み込み、値の妥当性を検証する。
    pub fn load(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let cfg: Config = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.general.symbols.is_empty() {
            return Err(ConfigError::Invalid("symbols が空です".into()));
        }
        if self.general.orderbook_depth == 0 {
            return Err(ConfigError::Invalid("orderbook_depth は 1 以上".into()));
        }
        if self.general.book_channel_capacity == 0 {
            return Err(ConfigError::Invalid(
                "book_channel_capacity は 1 以上".into(),
            ));
        }
        if self.recording.snapshot_channel_capacity == 0 {
            return Err(ConfigError::Invalid(
                "snapshot_channel_capacity は 1 以上".into(),
            ));
        }
        if self.enabled_dexes().is_empty() {
            return Err(ConfigError::Invalid(
                "少なくとも 1 つの DEX を enabled にしてください".into(),
            ));
        }
        if self.dex.hyperliquid.enabled && self.dex.hyperliquid.ping_interval_secs == 0 {
            return Err(ConfigError::Invalid(
                "hyperliquid.ping_interval_secs は 1 以上".into(),
            ));
        }
        if self.dex.edgex.enabled && self.dex.edgex.depth_level == 0 {
            return Err(ConfigError::Invalid("edgex.depth_level は 1 以上".into()));
        }
        if self.dex.aster.enabled && self.dex.aster.snapshot_limit == 0 {
            return Err(ConfigError::Invalid(
                "aster.snapshot_limit は 1 以上".into(),
            ));
        }
        if self.dex.lighter.enabled && self.dex.lighter.keepalive_interval_secs == 0 {
            return Err(ConfigError::Invalid(
                "lighter.keepalive_interval_secs は 1 以上".into(),
            ));
        }
        // Lighter は 2 分間フレームを送らないと切断される
        if self.dex.lighter.enabled && self.dex.lighter.keepalive_interval_secs >= 120 {
            return Err(ConfigError::Invalid(
                "lighter.keepalive_interval_secs は 120 未満にしてください（2 分で切断されます）"
                    .into(),
            ));
        }
        for (dex, symbols) in Dex::ALL
            .iter()
            .filter(|d| self.is_enabled(**d))
            .map(|d| (*d, self.symbols_for(*d)))
        {
            if symbols.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "{dex} で購読する銘柄が 1 つもありません（excluded_symbols を確認してください）"
                )));
            }
        }
        Ok(())
    }

    /// VWAP 乖離の計算に使うノーショナル。0 以下なら計算しない。
    pub fn vwap_notional(&self) -> Option<Decimal> {
        if self.general.vwap_notional_usd > Decimal::ZERO {
            Some(self.general.vwap_notional_usd)
        } else {
            None
        }
    }

    pub fn is_enabled(&self, dex: Dex) -> bool {
        match dex {
            Dex::Hyperliquid => self.dex.hyperliquid.enabled,
            Dex::EdgeX => self.dex.edgex.enabled,
            Dex::Aster => self.dex.aster.enabled,
            Dex::Lighter => self.dex.lighter.enabled,
        }
    }

    /// 有効な DEX を [`Dex::ALL`] の順で返す。ペア列挙の入力になる。
    pub fn enabled_dexes(&self) -> Vec<Dex> {
        Dex::ALL
            .into_iter()
            .filter(|d| self.is_enabled(*d))
            .collect()
    }

    /// その DEX で実際に購読する銘柄（`excluded_symbols` を除いたもの）。
    ///
    /// 「全銘柄が全 DEX に存在する」前提を置かないための設定側の入口。
    pub fn symbols_for(&self, dex: Dex) -> Vec<Symbol> {
        let excluded: &[Symbol] = match dex {
            Dex::Hyperliquid => &self.dex.hyperliquid.excluded_symbols,
            Dex::EdgeX => &self.dex.edgex.excluded_symbols,
            Dex::Aster => &self.dex.aster.excluded_symbols,
            Dex::Lighter => &self.dex.lighter.excluded_symbols,
        };
        self.general
            .symbols
            .iter()
            .filter(|s| !excluded.contains(s))
            .copied()
            .collect()
    }
}

impl HyperliquidConfig {
    pub fn reconnect_policy(&self) -> ReconnectPolicy {
        ReconnectPolicy {
            max_attempts: self.reconnect_max_attempts,
            base_delay_ms: self.reconnect_base_delay_ms,
            max_delay_ms: self.reconnect_max_delay_ms,
        }
    }
}

impl EdgeXConfig {
    pub fn reconnect_policy(&self) -> ReconnectPolicy {
        ReconnectPolicy {
            max_attempts: self.reconnect_max_attempts,
            base_delay_ms: self.reconnect_base_delay_ms,
            max_delay_ms: self.reconnect_max_delay_ms,
        }
    }
}

impl AsterConfig {
    pub fn reconnect_policy(&self) -> ReconnectPolicy {
        ReconnectPolicy {
            max_attempts: self.reconnect_max_attempts,
            base_delay_ms: self.reconnect_base_delay_ms,
            max_delay_ms: self.reconnect_max_delay_ms,
        }
    }

    /// `<symbol>@depth@100ms` 形式のストリーム名（全て小文字）。
    pub fn depth_stream_name(&self, symbol: Symbol) -> String {
        let base = symbol.to_stream_symbol(Dex::Aster);
        let interval = self.depth_stream_interval.trim();
        if interval.is_empty() {
            format!("{base}@depth")
        } else {
            format!("{base}@depth@{interval}")
        }
    }

    /// REST 板スナップショットの URL。
    pub fn snapshot_url(&self, symbol: Symbol) -> String {
        format!(
            "{}/fapi/v1/depth?symbol={}&limit={}",
            self.rest_url.trim_end_matches('/'),
            symbol.to_dex_symbol(Dex::Aster),
            self.snapshot_limit
        )
    }

    /// 24 時間の強制切断より前に張り直すための上限稼働時間。0 なら無効。
    pub fn session_max_duration(&self) -> Option<std::time::Duration> {
        if self.reconnect_before_hours == 0 {
            None
        } else {
            Some(std::time::Duration::from_secs(
                self.reconnect_before_hours * 3_600,
            ))
        }
    }
}

impl LighterConfig {
    pub fn reconnect_policy(&self) -> ReconnectPolicy {
        ReconnectPolicy {
            max_attempts: self.reconnect_max_attempts,
            base_delay_ms: self.reconnect_base_delay_ms,
            max_delay_ms: self.reconnect_max_delay_ms,
        }
    }

    /// `use_testnet` に応じた接続先。
    pub fn effective_ws_url(&self) -> &str {
        if self.use_testnet {
            &self.testnet_ws_url
        } else {
            &self.ws_url
        }
    }
}

fn default_symbols() -> Vec<Symbol> {
    Symbol::ALL.to_vec()
}
fn default_depth() -> usize {
    20
}
fn default_vwap_notional() -> Decimal {
    Decimal::from(1_000)
}
fn default_book_channel_capacity() -> usize {
    4_096
}
fn default_true() -> bool {
    true
}
fn default_hl_ws_url() -> String {
    "wss://api.hyperliquid.xyz/ws".to_string()
}
fn default_edgex_ws_url() -> String {
    "wss://quote.edgex.exchange/api/v1/public/ws".to_string()
}
fn default_edgex_metadata_url() -> String {
    "https://pro.edgex.exchange/api/v1/public/meta/getMetaData".to_string()
}
fn default_reconnect_max_attempts() -> u32 {
    10
}
fn default_reconnect_base_delay_ms() -> u64 {
    500
}
fn default_reconnect_max_delay_ms() -> u64 {
    30_000
}
fn default_hl_ping_interval_secs() -> u64 {
    30
}
fn default_edgex_ping_interval_secs() -> u64 {
    20
}
fn default_edgex_depth_level() -> usize {
    15
}
fn default_edgex_resync_interval_secs() -> u64 {
    300
}
fn default_aster_ws_url() -> String {
    "wss://fstream.asterdex.com".to_string()
}
fn default_aster_rest_url() -> String {
    // Spot 用の sapi ではなく Futures/Perp 用の fapi を使う
    "https://fapi.asterdex.com".to_string()
}
fn default_aster_depth_interval() -> String {
    "100ms".to_string()
}
fn default_aster_snapshot_limit() -> u32 {
    1_000
}
fn default_aster_reconnect_before_hours() -> u64 {
    23
}
fn default_aster_resync_backoff_ms() -> u64 {
    5_000
}
fn default_lighter_ws_url() -> String {
    "wss://mainnet.zklighter.elliot.ai/stream".to_string()
}
fn default_lighter_testnet_ws_url() -> String {
    "wss://testnet.zklighter.elliot.ai/stream".to_string()
}
fn default_lighter_keepalive_secs() -> u64 {
    60
}
fn default_lighter_mapping_warn_secs() -> u64 {
    30
}
fn default_csv_dir() -> PathBuf {
    PathBuf::from("data")
}
fn default_log_dir() -> PathBuf {
    PathBuf::from("logs")
}
fn default_csv_mode() -> CsvMode {
    CsvMode::All
}
fn default_csv_sample_interval_ms() -> u64 {
    100
}
fn default_csv_threshold_bps() -> Decimal {
    Decimal::from(5)
}
fn default_csv_flush_interval_ms() -> u64 {
    1_000
}
fn default_snapshot_channel_capacity() -> usize {
    8_192
}
fn default_log_filter() -> String {
    "info".to_string()
}
fn default_latency_warn_ms() -> i64 {
    500
}
fn default_staleness_warn_ms() -> i64 {
    1_000
}
fn default_status_interval_secs() -> u64 {
    60
}
fn default_warn_min_interval_secs() -> u64 {
    10
}

impl Default for GeneralConfig {
    fn default() -> Self {
        GeneralConfig {
            symbols: default_symbols(),
            orderbook_depth: default_depth(),
            vwap_notional_usd: default_vwap_notional(),
            book_channel_capacity: default_book_channel_capacity(),
        }
    }
}

impl Default for HyperliquidConfig {
    fn default() -> Self {
        HyperliquidConfig {
            enabled: true,
            ws_url: default_hl_ws_url(),
            reconnect_max_attempts: default_reconnect_max_attempts(),
            reconnect_base_delay_ms: default_reconnect_base_delay_ms(),
            reconnect_max_delay_ms: default_reconnect_max_delay_ms(),
            ping_interval_secs: default_hl_ping_interval_secs(),
            excluded_symbols: Vec::new(),
        }
    }
}

impl Default for EdgeXConfig {
    fn default() -> Self {
        EdgeXConfig {
            enabled: true,
            ws_url: default_edgex_ws_url(),
            reconnect_max_attempts: default_reconnect_max_attempts(),
            reconnect_base_delay_ms: default_reconnect_base_delay_ms(),
            reconnect_max_delay_ms: default_reconnect_max_delay_ms(),
            ping_interval_secs: default_edgex_ping_interval_secs(),
            depth_level: default_edgex_depth_level(),
            resync_interval_secs: default_edgex_resync_interval_secs(),
            resolve_contract_ids: true,
            metadata_url: default_edgex_metadata_url(),
            contract_ids: BTreeMap::new(),
            excluded_symbols: Vec::new(),
        }
    }
}

impl Default for AsterConfig {
    fn default() -> Self {
        AsterConfig {
            enabled: true,
            ws_url: default_aster_ws_url(),
            rest_url: default_aster_rest_url(),
            depth_stream_interval: default_aster_depth_interval(),
            snapshot_limit: default_aster_snapshot_limit(),
            reconnect_before_hours: default_aster_reconnect_before_hours(),
            reconnect_max_attempts: default_reconnect_max_attempts(),
            reconnect_base_delay_ms: default_reconnect_base_delay_ms(),
            reconnect_max_delay_ms: default_reconnect_max_delay_ms(),
            resync_backoff_ms: default_aster_resync_backoff_ms(),
            excluded_symbols: Vec::new(),
        }
    }
}

impl Default for LighterConfig {
    fn default() -> Self {
        LighterConfig {
            enabled: true,
            ws_url: default_lighter_ws_url(),
            testnet_ws_url: default_lighter_testnet_ws_url(),
            use_testnet: false,
            keepalive_interval_secs: default_lighter_keepalive_secs(),
            reconnect_max_attempts: default_reconnect_max_attempts(),
            reconnect_base_delay_ms: default_reconnect_base_delay_ms(),
            reconnect_max_delay_ms: default_reconnect_max_delay_ms(),
            mapping_warn_secs: default_lighter_mapping_warn_secs(),
            excluded_symbols: Vec::new(),
        }
    }
}

impl Default for RecordingConfig {
    fn default() -> Self {
        RecordingConfig {
            csv_dir: default_csv_dir(),
            log_dir: default_log_dir(),
            csv_mode: default_csv_mode(),
            csv_sample_interval_ms: default_csv_sample_interval_ms(),
            csv_threshold_bps: default_csv_threshold_bps(),
            csv_flush_interval_ms: default_csv_flush_interval_ms(),
            snapshot_channel_capacity: default_snapshot_channel_capacity(),
            log_filter: default_log_filter(),
            log_to_stdout: false,
        }
    }
}

impl Default for MonitoringConfig {
    fn default() -> Self {
        MonitoringConfig {
            latency_warn_threshold_ms: default_latency_warn_ms(),
            staleness_warn_threshold_ms: default_staleness_warn_ms(),
            status_interval_secs: default_status_interval_secs(),
            warn_min_interval_secs: default_warn_min_interval_secs(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn parses_documented_example() {
        let toml_src = r#"
[general]
symbols = ["BTC", "ETH", "SOL", "HYPE"]
orderbook_depth = 20

[dex.hyperliquid]
enabled = true
ws_url = "wss://api.hyperliquid.xyz/ws"
reconnect_max_attempts = 10
reconnect_base_delay_ms = 500

[dex.edgex]
enabled = true
ws_url = "wss://quote.edgex.exchange/api/v1/public/ws"
reconnect_max_attempts = 10
reconnect_base_delay_ms = 500
contract_ids = { BTC = "10000001" }

[recording]
csv_dir = "data"
log_dir = "logs"
csv_mode = "all"
csv_sample_interval_ms = 100
csv_threshold_bps = 5

[monitoring]
latency_warn_threshold_ms = 500
staleness_warn_threshold_ms = 1000
"#;
        let cfg: Config = toml::from_str(toml_src).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.general.symbols.len(), 4);
        assert_eq!(cfg.general.orderbook_depth, 20);
        assert_eq!(cfg.recording.csv_mode, CsvMode::All);
        assert_eq!(
            cfg.dex
                .edgex
                .contract_ids
                .get(&Symbol::Btc)
                .map(String::as_str),
            Some("10000001")
        );
        // 省略した項目はデフォルトで埋まる
        assert_eq!(cfg.dex.hyperliquid.ping_interval_secs, 30);
        assert_eq!(cfg.general.vwap_notional_usd, Decimal::from(1_000));
    }

    #[test]
    fn empty_toml_yields_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.general.symbols, Symbol::ALL.to_vec());
    }

    #[test]
    fn rejects_unknown_keys() {
        let err = toml::from_str::<Config>("[general]\nsymbolz = []\n").unwrap_err();
        assert!(err.to_string().contains("symbolz"), "{err}");
    }

    #[test]
    fn rejects_invalid_values() {
        let mut cfg = Config::default();
        cfg.general.symbols.clear();
        assert!(cfg.validate().is_err());

        let mut cfg = Config::default();
        cfg.general.orderbook_depth = 0;
        assert!(cfg.validate().is_err());

        let mut cfg = Config::default();
        cfg.dex.hyperliquid.enabled = false;
        cfg.dex.edgex.enabled = false;
        cfg.dex.aster.enabled = false;
        cfg.dex.lighter.enabled = false;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn csv_mode_parses_all_variants() {
        for (raw, expected) in [
            ("all", CsvMode::All),
            ("sampled", CsvMode::Sampled),
            ("threshold", CsvMode::Threshold),
        ] {
            let cfg: Config =
                toml::from_str(&format!("[recording]\ncsv_mode = \"{raw}\"\n")).unwrap();
            assert_eq!(cfg.recording.csv_mode, expected);
        }
    }

    #[test]
    fn parses_aster_and_lighter_sections() {
        let toml_src = r#"
[dex.aster]
enabled = true
ws_url = "wss://fstream.asterdex.com"
rest_url = "https://fapi.asterdex.com"
depth_stream_interval = "100ms"
snapshot_limit = 1000
reconnect_before_hours = 23
reconnect_max_attempts = 10
reconnect_base_delay_ms = 500
resync_backoff_ms = 5000
excluded_symbols = []

[dex.lighter]
enabled = true
ws_url = "wss://mainnet.zklighter.elliot.ai/stream"
testnet_ws_url = "wss://testnet.zklighter.elliot.ai/stream"
use_testnet = false
keepalive_interval_secs = 60
reconnect_max_attempts = 10
reconnect_base_delay_ms = 500
excluded_symbols = []
"#;
        let cfg: Config = toml::from_str(toml_src).unwrap();
        cfg.validate().unwrap();

        assert_eq!(cfg.dex.aster.snapshot_limit, 1000);
        assert_eq!(
            cfg.dex.aster.session_max_duration(),
            Some(std::time::Duration::from_secs(23 * 3600))
        );
        assert_eq!(
            cfg.dex.lighter.effective_ws_url(),
            "wss://mainnet.zklighter.elliot.ai/stream"
        );
        assert_eq!(cfg.enabled_dexes(), Dex::ALL.to_vec());
    }

    #[test]
    fn aster_stream_and_snapshot_urls() {
        let cfg = AsterConfig::default();
        assert_eq!(cfg.depth_stream_name(Symbol::Btc), "btcusdt@depth@100ms");
        assert_eq!(
            cfg.snapshot_url(Symbol::Hype),
            "https://fapi.asterdex.com/fapi/v1/depth?symbol=HYPEUSDT&limit=1000"
        );

        // 間隔を空にすると既定速度のストリームになる
        let cfg = AsterConfig {
            depth_stream_interval: String::new(),
            ..Default::default()
        };
        assert_eq!(cfg.depth_stream_name(Symbol::Eth), "ethusdt@depth");
    }

    #[test]
    fn lighter_switches_to_testnet() {
        let cfg = LighterConfig {
            use_testnet: true,
            ..Default::default()
        };
        assert_eq!(
            cfg.effective_ws_url(),
            "wss://testnet.zklighter.elliot.ai/stream"
        );
    }

    #[test]
    fn excluded_symbols_are_filtered_per_dex() {
        let cfg: Config = toml::from_str(
            r#"
[general]
symbols = ["BTC", "ETH", "HYPE"]

[dex.lighter]
excluded_symbols = ["HYPE"]
"#,
        )
        .unwrap();
        cfg.validate().unwrap();

        assert_eq!(
            cfg.symbols_for(Dex::Lighter),
            vec![Symbol::Btc, Symbol::Eth]
        );
        assert_eq!(
            cfg.symbols_for(Dex::Aster),
            vec![Symbol::Btc, Symbol::Eth, Symbol::Hype]
        );
    }

    #[test]
    fn enabled_dexes_reflects_flags() {
        let mut cfg = Config::default();
        cfg.dex.edgex.enabled = false;
        cfg.dex.lighter.enabled = false;
        assert_eq!(cfg.enabled_dexes(), vec![Dex::Hyperliquid, Dex::Aster]);
        cfg.validate().unwrap();

        cfg.dex.hyperliquid.enabled = false;
        cfg.dex.aster.enabled = false;
        assert!(cfg.validate().is_err(), "全 DEX 無効は不正");
    }

    #[test]
    fn rejects_dangerous_lighter_keepalive() {
        let mut cfg = Config::default();
        // 2 分以上あけるとサーバーに切断される
        cfg.dex.lighter.keepalive_interval_secs = 120;
        assert!(cfg.validate().is_err());

        cfg.dex.lighter.keepalive_interval_secs = 119;
        cfg.validate().unwrap();
    }

    #[test]
    fn rejects_excluding_every_symbol() {
        let mut cfg = Config::default();
        cfg.dex.aster.excluded_symbols = Symbol::ALL.to_vec();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn vwap_notional_disabled_when_zero() {
        let mut cfg = Config::default();
        cfg.general.vwap_notional_usd = Decimal::ZERO;
        assert!(cfg.vwap_notional().is_none());
    }
}
