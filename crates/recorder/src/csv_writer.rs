use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use config::{CsvMode, RecordingConfig};
use core_types::Symbol;
use market_data::DivergenceSnapshot;
use rust_decimal::Decimal;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

/// CSV のカラム定義（フェーズ1 設計書 §6 に対応）。
pub const CSV_HEADER: &[&str] = &[
    "timestamp_ms",
    "symbol",
    "dex_a",
    "dex_b",
    "mid_a",
    "mid_b",
    "best_bid_a",
    "best_ask_a",
    "best_bid_b",
    "best_ask_b",
    "raw_spread_bps",
    "executable_spread_bps",
    "vwap_spread_bps",
    "depth_a_bps10",
    "depth_b_bps10",
    "staleness_delta_ms",
    "latency_a_ms",
    "latency_b_ms",
    "pipeline_latency_us",
];

/// bps 値の小数桁数。丸めないと 28 桁の Decimal がそのまま出てファイルが膨らむ。
const BPS_SCALE: u32 = 4;

/// 銘柄ごと・日次でファイルを分けて CSV を書き出す。
///
/// 開いたままのファイルハンドルは (当日 × 銘柄数) 個に限られ、日付が変われば
/// 古いハンドルは閉じられるため、24 時間以上の連続稼働でも増え続けない。
pub struct CsvRecorder {
    dir: PathBuf,
    mode: CsvMode,
    sample_interval_ms: u64,
    threshold_bps: Decimal,
    writers: HashMap<(NaiveDate, Symbol), csv::Writer<BufWriter<File>>>,
    /// 銘柄ごとの最終書き込み時刻（`CsvMode::Sampled` 用, wall clock ms）。
    last_written_ms: HashMap<Symbol, u64>,
    rows_written: u64,
    rows_skipped: u64,
}

impl CsvRecorder {
    pub fn new(cfg: &RecordingConfig) -> Self {
        CsvRecorder {
            dir: cfg.csv_dir.clone(),
            mode: cfg.csv_mode,
            sample_interval_ms: cfg.csv_sample_interval_ms,
            threshold_bps: cfg.csv_threshold_bps,
            writers: HashMap::new(),
            last_written_ms: HashMap::new(),
            rows_written: 0,
            rows_skipped: 0,
        }
    }

    pub fn rows_written(&self) -> u64 {
        self.rows_written
    }

    pub fn rows_skipped(&self) -> u64 {
        self.rows_skipped
    }

    /// 記録モードに従って、このスナップショットを書くべきか判定する。
    ///
    /// `Sampled` の場合は判定時に最終書き込み時刻を更新するため、
    /// 「書く」と判定したら実際に書くこと。
    pub fn should_record(&mut self, snap: &DivergenceSnapshot) -> bool {
        match self.mode {
            CsvMode::All => true,
            CsvMode::Sampled => {
                let last = self.last_written_ms.get(&snap.symbol).copied();
                match last {
                    Some(prev)
                        if snap.computed_at_wall_ms.saturating_sub(prev)
                            < self.sample_interval_ms =>
                    {
                        false
                    }
                    _ => {
                        self.last_written_ms
                            .insert(snap.symbol, snap.computed_at_wall_ms);
                        true
                    }
                }
            }
            // 「取れる方向の乖離」が閾値を超えたものだけを残す。
            CsvMode::Threshold => snap.executable_spread_bps.abs() >= self.threshold_bps,
        }
    }

    /// スナップショットを 1 行書き込む（モード判定込み）。
    pub fn record(&mut self, snap: &DivergenceSnapshot) -> Result<(), CsvError> {
        if !self.should_record(snap) {
            self.rows_skipped += 1;
            return Ok(());
        }
        let date = wall_ms_to_date(snap.computed_at_wall_ms);
        let writer = self.writer_for(date, snap.symbol)?;
        writer.write_record(snapshot_to_row(snap))?;
        self.rows_written += 1;
        Ok(())
    }

    fn writer_for(
        &mut self,
        date: NaiveDate,
        symbol: Symbol,
    ) -> Result<&mut csv::Writer<BufWriter<File>>, CsvError> {
        if !self.writers.contains_key(&(date, symbol)) {
            // 日付が変わったら前日のハンドルは閉じる（flush してから drop）。
            self.close_stale(date);
            let path = self.path_for(date, symbol);
            let writer = open_writer(&path)?;
            info!(file = %path.display(), symbol = %symbol, "CSV ファイルを開きました");
            self.writers.insert((date, symbol), writer);
        }
        Ok(self
            .writers
            .get_mut(&(date, symbol))
            .expect("直前に挿入済み"))
    }

    fn close_stale(&mut self, current: NaiveDate) {
        let stale: Vec<_> = self
            .writers
            .keys()
            .filter(|(d, _)| *d != current)
            .copied()
            .collect();
        for key in stale {
            if let Some(mut w) = self.writers.remove(&key) {
                if let Err(e) = w.flush() {
                    error!(error = %e, date = %key.0, symbol = %key.1, "CSV の flush に失敗");
                }
            }
        }
    }

    fn path_for(&self, date: NaiveDate, symbol: Symbol) -> PathBuf {
        self.dir
            .join(format!("{}_{}.csv", date.format("%Y-%m-%d"), symbol))
    }

    /// 全ファイルを flush する。
    pub fn flush(&mut self) -> Result<(), CsvError> {
        let mut first_error = None;
        for ((date, symbol), writer) in self.writers.iter_mut() {
            if let Err(e) = writer.flush() {
                error!(error = %e, date = %date, symbol = %symbol, "CSV の flush に失敗");
                first_error.get_or_insert(CsvError::Io(e));
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Drop for CsvRecorder {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CsvError {
    #[error("CSV の書き込みに失敗: {0}")]
    Csv(#[from] csv::Error),
    #[error("ファイル I/O に失敗: {0}")]
    Io(#[from] std::io::Error),
}

fn open_writer(path: &Path) -> Result<csv::Writer<BufWriter<File>>, CsvError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // 再起動しても同日のファイルを追記で引き継ぐ。ヘッダは新規作成時のみ書く。
    let is_new = !path.exists()
        || std::fs::metadata(path)
            .map(|m| m.len() == 0)
            .unwrap_or(true);
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut writer = csv::WriterBuilder::new()
        .flexible(false)
        .from_writer(BufWriter::new(file));
    if is_new {
        writer.write_record(CSV_HEADER)?;
        writer.flush()?;
    }
    Ok(writer)
}

fn wall_ms_to_date(wall_ms: u64) -> NaiveDate {
    DateTime::<Utc>::from_timestamp_millis(wall_ms as i64)
        .unwrap_or_else(Utc::now)
        .date_naive()
}

fn fmt_bps(v: Decimal) -> String {
    v.round_dp(BPS_SCALE).normalize().to_string()
}

fn fmt_opt<T: ToString>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}

/// スナップショットを [`CSV_HEADER`] と同じ順序の 1 行に変換する。
pub fn snapshot_to_row(snap: &DivergenceSnapshot) -> Vec<String> {
    vec![
        snap.computed_at_wall_ms.to_string(),
        snap.symbol.to_string(),
        snap.dex_a.to_string(),
        snap.dex_b.to_string(),
        snap.mid_a.0.normalize().to_string(),
        snap.mid_b.0.normalize().to_string(),
        snap.best_bid_a.0.normalize().to_string(),
        snap.best_ask_a.0.normalize().to_string(),
        snap.best_bid_b.0.normalize().to_string(),
        snap.best_ask_b.0.normalize().to_string(),
        fmt_bps(snap.raw_spread_bps),
        fmt_bps(snap.executable_spread_bps),
        fmt_opt(snap.vwap_spread_bps.map(fmt_bps)),
        snap.depth_a_bps10.0.normalize().to_string(),
        snap.depth_b_bps10.0.normalize().to_string(),
        snap.staleness_delta_ms.to_string(),
        fmt_opt(snap.exchange_latency_ms(snap.dex_a)),
        fmt_opt(snap.exchange_latency_ms(snap.dex_b)),
        fmt_opt(snap.pipeline_latency_us()),
    ]
}

/// CSV 書き込みタスクを起動する。
///
/// ディスク I/O を WS 受信タスクから切り離すため、必ず独立タスクとして動かす。
/// `rx` の送信側が全て drop されるとループを抜け、最後に flush して終了する。
pub fn spawn_csv_writer(
    cfg: &RecordingConfig,
    mut rx: mpsc::Receiver<DivergenceSnapshot>,
) -> JoinHandle<()> {
    let mut recorder = CsvRecorder::new(cfg);
    let flush_interval = Duration::from_millis(cfg.csv_flush_interval_ms.max(1));

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(flush_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // 連続する書き込みエラーでログを埋め尽くさないよう、初回のみ詳細を出す。
        let mut write_errors: u64 = 0;

        loop {
            tokio::select! {
                maybe_snap = rx.recv() => {
                    match maybe_snap {
                        Some(snap) => {
                            if let Err(e) = recorder.record(&snap) {
                                write_errors += 1;
                                if write_errors == 1 || write_errors % 1000 == 0 {
                                    error!(error = %e, count = write_errors, "CSV 書き込みエラー");
                                }
                            }
                        }
                        None => break,
                    }
                }
                _ = ticker.tick() => {
                    if let Err(e) = recorder.flush() {
                        warn!(error = %e, "定期 flush に失敗");
                    }
                }
            }
        }

        if let Err(e) = recorder.flush() {
            error!(error = %e, "終了時の flush に失敗");
        }
        info!(
            rows_written = recorder.rows_written(),
            rows_skipped = recorder.rows_skipped(),
            write_errors,
            "CSV writer タスク終了"
        );
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{Dex, Level, MessageTrace, OrderBook, Price, Quantity};
    use rust_decimal_macros::dec;

    fn snapshot(symbol: Symbol, wall_ms: u64, bid_a: Decimal) -> DivergenceSnapshot {
        let mk = |dex: Dex, bid: Decimal, ask: Decimal| {
            let mut trace = MessageTrace::on_receive();
            trace.received_wall_ms = wall_ms;
            trace.exchange_ts_ms = Some(wall_ms - 7);
            trace.mark_normalized();
            OrderBook::new(
                dex,
                symbol,
                // VWAP 列も埋まるよう、想定ノーショナル分の深さを持たせる
                vec![Level::new(Price(bid), Quantity(dec!(30)))],
                vec![Level::new(Price(ask), Quantity(dec!(30)))],
                trace,
            )
        };
        let a = mk(Dex::Hyperliquid, bid_a, bid_a + dec!(1));
        let b = mk(Dex::EdgeX, dec!(100), dec!(101));
        let mut snap =
            DivergenceSnapshot::compute(&a, &b, Dex::Hyperliquid, Some(dec!(1000))).unwrap();
        snap.computed_at_wall_ms = wall_ms;
        snap
    }

    fn recorder_in(dir: &Path, mode: CsvMode) -> CsvRecorder {
        let cfg = RecordingConfig {
            csv_dir: dir.to_path_buf(),
            csv_mode: mode,
            csv_sample_interval_ms: 100,
            csv_threshold_bps: dec!(5),
            ..Default::default()
        };
        CsvRecorder::new(&cfg)
    }

    fn read_csv(path: &Path) -> Vec<Vec<String>> {
        let content = std::fs::read_to_string(path).unwrap();
        content
            .lines()
            .map(|l| l.split(',').map(|s| s.to_string()).collect())
            .collect()
    }

    #[test]
    fn row_matches_header_length() {
        let snap = snapshot(Symbol::Btc, 1_700_000_000_000, dec!(110));
        assert_eq!(snapshot_to_row(&snap).len(), CSV_HEADER.len());
    }

    #[test]
    fn writes_header_once_and_appends_rows() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut rec = recorder_in(dir.path(), CsvMode::All);
            rec.record(&snapshot(Symbol::Btc, 1_700_000_000_000, dec!(110)))
                .unwrap();
            rec.record(&snapshot(Symbol::Btc, 1_700_000_000_500, dec!(111)))
                .unwrap();
            rec.flush().unwrap();
        }
        // 別インスタンス（= 再起動相当）でも同じファイルに追記され、ヘッダは重複しない
        {
            let mut rec = recorder_in(dir.path(), CsvMode::All);
            rec.record(&snapshot(Symbol::Btc, 1_700_000_001_000, dec!(112)))
                .unwrap();
            rec.flush().unwrap();
        }

        let path = dir.path().join("2023-11-14_BTC.csv");
        let rows = read_csv(&path);
        assert_eq!(rows.len(), 4, "header + 3 rows: {rows:?}");
        assert_eq!(rows[0], CSV_HEADER.to_vec());
        assert_eq!(rows[1][1], "BTC");
        assert_eq!(rows[1][0], "1700000000000");
    }

    #[test]
    fn splits_files_per_symbol_and_per_day() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(dir.path(), CsvMode::All);
        rec.record(&snapshot(Symbol::Btc, 1_700_000_000_000, dec!(110)))
            .unwrap();
        rec.record(&snapshot(Symbol::Eth, 1_700_000_000_000, dec!(110)))
            .unwrap();
        // 翌日 (+24h) → 別ファイル、かつ前日のハンドルは閉じられる
        rec.record(&snapshot(Symbol::Btc, 1_700_086_400_000, dec!(110)))
            .unwrap();
        rec.flush().unwrap();

        assert!(dir.path().join("2023-11-14_BTC.csv").exists());
        assert!(dir.path().join("2023-11-14_ETH.csv").exists());
        assert!(dir.path().join("2023-11-15_BTC.csv").exists());
        assert_eq!(rec.writers.len(), 1, "前日分のハンドルは閉じられている");
    }

    #[test]
    fn sampled_mode_thins_by_interval() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(dir.path(), CsvMode::Sampled);
        let base = 1_700_000_000_000;
        for offset in [0, 30, 60, 100, 130, 210] {
            rec.record(&snapshot(Symbol::Btc, base + offset, dec!(110)))
                .unwrap();
        }
        rec.flush().unwrap();
        // 0ms, 100ms, 210ms の 3 件が残る
        assert_eq!(rec.rows_written(), 3);
        assert_eq!(rec.rows_skipped(), 3);
    }

    #[test]
    fn sampled_mode_is_per_symbol() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(dir.path(), CsvMode::Sampled);
        let base = 1_700_000_000_000;
        rec.record(&snapshot(Symbol::Btc, base, dec!(110))).unwrap();
        // 別銘柄は独立に判定される
        rec.record(&snapshot(Symbol::Eth, base + 1, dec!(110)))
            .unwrap();
        rec.record(&snapshot(Symbol::Btc, base + 2, dec!(110)))
            .unwrap();
        assert_eq!(rec.rows_written(), 2);
        assert_eq!(rec.rows_skipped(), 1);
    }

    #[test]
    fn threshold_mode_keeps_only_large_divergence() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(dir.path(), CsvMode::Threshold);
        // A も B も 100/101 → executable は -99bps で |値| > 5 → 記録される
        rec.record(&snapshot(Symbol::Btc, 1_700_000_000_000, dec!(100)))
            .unwrap();
        assert_eq!(rec.rows_written(), 1);

        // 閾値を 200bps に上げると同じスナップショットは弾かれる
        let cfg = RecordingConfig {
            csv_dir: dir.path().to_path_buf(),
            csv_mode: CsvMode::Threshold,
            csv_threshold_bps: dec!(200),
            ..Default::default()
        };
        let mut rec = CsvRecorder::new(&cfg);
        rec.record(&snapshot(Symbol::Btc, 1_700_000_000_000, dec!(100)))
            .unwrap();
        assert_eq!(rec.rows_written(), 0);
        assert_eq!(rec.rows_skipped(), 1);
    }

    #[test]
    fn latency_columns_are_populated() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(dir.path(), CsvMode::All);
        rec.record(&snapshot(Symbol::Sol, 1_700_000_000_000, dec!(110)))
            .unwrap();
        rec.flush().unwrap();

        let rows = read_csv(&dir.path().join("2023-11-14_SOL.csv"));
        let header: Vec<&str> = CSV_HEADER.to_vec();
        let idx = |name: &str| header.iter().position(|h| *h == name).unwrap();
        assert_eq!(rows[1][idx("latency_a_ms")], "7");
        assert_eq!(rows[1][idx("latency_b_ms")], "7");
        assert_eq!(rows[1][idx("staleness_delta_ms")], "0");
        // トリガー側のみ内部処理時間が入る
        assert!(!rows[1][idx("pipeline_latency_us")].is_empty());
        assert!(!rows[1][idx("vwap_spread_bps")].is_empty());
    }

    #[test]
    fn bps_values_are_rounded() {
        assert_eq!(fmt_bps(dec!(-99.5024875621890547)), "-99.5025");
        assert_eq!(fmt_bps(dec!(0.00000001)), "0");
        assert_eq!(fmt_bps(dec!(12.5000)), "12.5");
    }
}
