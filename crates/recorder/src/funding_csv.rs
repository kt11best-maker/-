//! ファンディングレートの CSV 出力。
//!
//! # 価格差 CSV と構造が違う点
//!
//! - 価格差 CSV: **1 行 = 1 ペアの比較**（`dex_a` / `dex_b`）
//! - ファンディング CSV: **1 行 = 1 DEX の状態**（`dex` 1 列）
//!
//! ファンディングは更新頻度が低く、ペアで持つと同じ値が冗長に並ぶ。ペア比較は
//! 分析時に行う。
//!
//! # 記録頻度
//!
//! - `current_rate` が**変化したときだけ**書く（板と違い、ほとんど変わらない）
//! - ただし値が変わらなくても `heartbeat_interval_secs` ごとに 1 行残す。
//!   **データの欠損と bot の停止を区別できるようにするため。**

use std::collections::HashMap;
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::time::Duration;

use chrono::NaiveDate;
use config::RecordingConfig;
use core_types::{Dex, FundingRate, Symbol};
use rust_decimal::Decimal;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::csv_writer::{fmt_opt, open_writer_with_header, wall_ms_to_date, CsvError};

/// ファンディング CSV のカラム定義（ファンディング収集設計書 §5 に対応）。
pub const FUNDING_CSV_HEADER: &[&str] = &[
    "timestamp_ms",
    "symbol",
    // ペアではなく単一 DEX。ここが価格差 CSV と違う点。
    "dex",
    "current_rate",
    "predicted_rate",
    "interval_hours",
    // DEX 間比較はこの列で行う（精算間隔の違いを吸収済み）
    "annualized_pct",
    "next_funding_time_ms",
    "index_price",
    "mark_price",
    "exchange_ts_ms",
    "latency_ms",
    // 流動性指標。ファンディングと同じメッセージで届くので同じ行に並べる
    // （分析時に時刻を突き合わせずに済む）。
    "open_interest",
    "volume_24h_usd",
    // 出来高 / OI（どちらも USD 換算）。**高すぎる場合は回転売買の疑い。**
    "volume_oi_ratio",
];

/// レート値の小数桁数。レート自体は 1e-7 オーダーなので広めに取る。
const RATE_SCALE: u32 = 12;
/// 年率（%）の小数桁数。
const PCT_SCALE: u32 = 6;
/// 出来高 / OI 比率の小数桁数。
const RATIO_SCALE: u32 = 6;

/// 直近に記録した内容（変化検知と心拍の判定に使う）。
#[derive(Debug, Clone, Copy)]
struct LastRecord {
    current_rate: Decimal,
    written_at_wall_ms: u64,
}

/// 銘柄ごと・日次でファイルを分けてファンディングレートを書き出す。
pub struct FundingRecorder {
    dir: PathBuf,
    heartbeat_ms: u64,
    writers: HashMap<(NaiveDate, Symbol), csv::Writer<BufWriter<File>>>,
    /// (DEX × 銘柄) ごとの直近記録。キー数は固定で増え続けない。
    last: HashMap<(Dex, Symbol), LastRecord>,
    rows_written: u64,
    rows_skipped: u64,
}

impl FundingRecorder {
    pub fn new(cfg: &RecordingConfig) -> Self {
        FundingRecorder {
            dir: cfg.csv_dir.clone(),
            heartbeat_ms: cfg.funding.heartbeat_interval_secs.saturating_mul(1_000),
            writers: HashMap::new(),
            last: HashMap::new(),
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

    /// このレートを書くべきか判定する。
    ///
    /// 「書く」と判定した時点で直近記録を更新するため、判定したら実際に書くこと。
    pub fn should_record(&mut self, rate: &FundingRate) -> bool {
        let key = (rate.dex, rate.symbol);
        let now_ms = rate.trace.received_wall_ms;

        let write = match self.last.get(&key) {
            // 初回は必ず記録する
            None => true,
            Some(prev) => {
                prev.current_rate != rate.current_rate
                    // 心拍: 値が変わらなくても最低これだけの間隔で 1 行残す
                    || now_ms.saturating_sub(prev.written_at_wall_ms) >= self.heartbeat_ms
            }
        };

        if write {
            self.last.insert(
                key,
                LastRecord {
                    current_rate: rate.current_rate,
                    written_at_wall_ms: now_ms,
                },
            );
        }
        write
    }

    /// レートを 1 行書き込む（記録頻度の判定込み）。
    pub fn record(&mut self, rate: &FundingRate) -> Result<(), CsvError> {
        if !self.should_record(rate) {
            self.rows_skipped += 1;
            return Ok(());
        }
        let date = wall_ms_to_date(rate.trace.received_wall_ms);
        let writer = self.writer_for(date, rate.symbol)?;
        writer.write_record(funding_to_row(rate))?;
        self.rows_written += 1;
        Ok(())
    }

    fn writer_for(
        &mut self,
        date: NaiveDate,
        symbol: Symbol,
    ) -> Result<&mut csv::Writer<BufWriter<File>>, CsvError> {
        if !self.writers.contains_key(&(date, symbol)) {
            self.close_stale(date);
            let path = self.dir.join(format!(
                "{}_{}_funding.csv",
                date.format("%Y-%m-%d"),
                symbol
            ));
            let writer = open_writer_with_header(&path, FUNDING_CSV_HEADER)?;
            info!(file = %path.display(), symbol = %symbol, "ファンディング CSV を開きました");
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
                    error!(error = %e, date = %key.0, symbol = %key.1, "ファンディング CSV の flush に失敗");
                }
            }
        }
    }

    pub fn flush(&mut self) -> Result<(), CsvError> {
        let mut first_error = None;
        for ((date, symbol), writer) in self.writers.iter_mut() {
            if let Err(e) = writer.flush() {
                error!(error = %e, date = %date, symbol = %symbol, "ファンディング CSV の flush に失敗");
                first_error.get_or_insert(CsvError::Io(e));
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Drop for FundingRecorder {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

fn fmt_rate(v: Decimal) -> String {
    v.round_dp(RATE_SCALE).normalize().to_string()
}

/// レートを [`FUNDING_CSV_HEADER`] と同じ順序の 1 行に変換する。
pub fn funding_to_row(rate: &FundingRate) -> Vec<String> {
    vec![
        rate.trace.received_wall_ms.to_string(),
        rate.symbol.to_string(),
        rate.dex.to_string(),
        fmt_rate(rate.current_rate),
        fmt_opt(rate.predicted_rate.map(fmt_rate)),
        fmt_opt(rate.interval_hours.map(|h| h.normalize().to_string())),
        fmt_opt(
            rate.annualized_pct()
                .map(|p| p.round_dp(PCT_SCALE).normalize().to_string()),
        ),
        fmt_opt(rate.next_funding_time_ms),
        fmt_opt(rate.index_price.map(|p| p.0.normalize().to_string())),
        fmt_opt(rate.mark_price.map(|p| p.0.normalize().to_string())),
        fmt_opt(rate.trace.exchange_ts_ms),
        fmt_opt(rate.latency_ms()),
        fmt_opt(rate.open_interest.map(|q| q.0.normalize().to_string())),
        fmt_opt(rate.volume_24h_usd.map(|v| v.normalize().to_string())),
        // 価格が無くて OI をノーショナル換算できない場合は空欄
        // （数量と USD を割った無意味な値を残さない）。
        fmt_opt(
            rate.market_stats()
                .volume_oi_ratio()
                .map(|r| r.round_dp(RATIO_SCALE).normalize().to_string()),
        ),
    ]
}

/// ファンディング CSV の書き込みタスクを起動する。
///
/// **板の収集とは独立したタスク。** ここが詰まっても止まっても、板の収集と
/// 価格差 CSV には影響しない。
pub fn spawn_funding_csv_writer(
    cfg: &RecordingConfig,
    mut rx: mpsc::Receiver<FundingRate>,
) -> JoinHandle<()> {
    let mut recorder = FundingRecorder::new(cfg);
    let flush_interval = Duration::from_millis(cfg.csv_flush_interval_ms.max(1));

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(flush_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut write_errors: u64 = 0;

        loop {
            tokio::select! {
                maybe_rate = rx.recv() => {
                    match maybe_rate {
                        Some(rate) => {
                            if let Err(e) = recorder.record(&rate) {
                                write_errors += 1;
                                if write_errors == 1 || write_errors % 100 == 0 {
                                    error!(error = %e, count = write_errors, "ファンディング CSV 書き込みエラー");
                                }
                            }
                        }
                        None => break,
                    }
                }
                _ = ticker.tick() => {
                    if let Err(e) = recorder.flush() {
                        warn!(error = %e, "ファンディング CSV の定期 flush に失敗");
                    }
                }
            }
        }

        if let Err(e) = recorder.flush() {
            error!(error = %e, "終了時のファンディング CSV flush に失敗");
        }
        info!(
            rows_written = recorder.rows_written(),
            rows_skipped = recorder.rows_skipped(),
            write_errors,
            "ファンディング CSV writer タスク終了"
        );
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::FundingRecordingConfig;
    use core_types::{MessageTrace, Price, Quantity};
    use rust_decimal_macros::dec;
    use std::path::Path;

    const BASE_MS: u64 = 1_700_000_000_000;

    fn rate(dex: Dex, symbol: Symbol, current: Decimal, wall_ms: u64) -> FundingRate {
        let mut trace = MessageTrace::on_receive();
        trace.received_wall_ms = wall_ms;
        trace.exchange_ts_ms = Some(wall_ms - 12);
        trace.mark_normalized();
        FundingRate {
            dex,
            symbol,
            current_rate: current,
            predicted_rate: Some(current + dec!(0.000001)),
            interval_hours: Some(dec!(1)),
            next_funding_time_ms: Some(wall_ms + 3_600_000),
            index_price: Some(Price(dec!(36000.0))),
            mark_price: Some(Price(dec!(36000.5))),
            open_interest: None,
            volume_24h_usd: None,
            trace,
        }
    }

    fn recorder_in(dir: &Path, heartbeat_secs: u64) -> FundingRecorder {
        let cfg = RecordingConfig {
            csv_dir: dir.to_path_buf(),
            funding: FundingRecordingConfig {
                enabled: true,
                heartbeat_interval_secs: heartbeat_secs,
            },
            ..Default::default()
        };
        FundingRecorder::new(&cfg)
    }

    fn read_csv(path: &Path) -> Vec<Vec<String>> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| l.split(',').map(|s| s.to_string()).collect())
            .collect()
    }

    fn column(row: &[String], name: &str) -> String {
        let idx = FUNDING_CSV_HEADER.iter().position(|h| *h == name).unwrap();
        row[idx].clone()
    }

    #[test]
    fn row_matches_header_length() {
        let r = rate(Dex::Lighter, Symbol::Btc, dec!(0.0000125), BASE_MS);
        assert_eq!(funding_to_row(&r).len(), FUNDING_CSV_HEADER.len());
    }

    #[test]
    fn writes_one_row_per_dex_not_per_pair() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(dir.path(), 300);
        rec.record(&rate(
            Dex::Hyperliquid,
            Symbol::Btc,
            dec!(0.0000125),
            BASE_MS,
        ))
        .unwrap();
        rec.record(&rate(Dex::Lighter, Symbol::Btc, dec!(0.0000100), BASE_MS))
            .unwrap();
        rec.flush().unwrap();

        let rows = read_csv(&dir.path().join("2023-11-14_BTC_funding.csv"));
        assert_eq!(rows.len(), 3, "header + 2 rows: {rows:?}");
        assert_eq!(rows[0], FUNDING_CSV_HEADER.to_vec());
        assert_eq!(column(&rows[1], "dex"), "hyperliquid");
        assert_eq!(column(&rows[2], "dex"), "lighter");
    }

    #[test]
    fn records_only_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(dir.path(), 300);
        // 同じ値が連続したらスキップされる
        for offset in [0, 1_000, 2_000] {
            rec.record(&rate(
                Dex::Lighter,
                Symbol::Btc,
                dec!(0.0000125),
                BASE_MS + offset,
            ))
            .unwrap();
        }
        assert_eq!(rec.rows_written(), 1);
        assert_eq!(rec.rows_skipped(), 2);

        // 値が変われば記録される
        rec.record(&rate(
            Dex::Lighter,
            Symbol::Btc,
            dec!(0.0000130),
            BASE_MS + 3_000,
        ))
        .unwrap();
        assert_eq!(rec.rows_written(), 2);
    }

    #[test]
    fn heartbeat_forces_a_row_even_without_change() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(dir.path(), 300);
        let same = dec!(0.0000125);

        rec.record(&rate(Dex::Lighter, Symbol::Btc, same, BASE_MS))
            .unwrap();
        // 299 秒後はまだ心拍未満
        rec.record(&rate(Dex::Lighter, Symbol::Btc, same, BASE_MS + 299_000))
            .unwrap();
        assert_eq!(rec.rows_written(), 1);

        // 300 秒経過で 1 行残す（欠損と停止を区別するため）
        rec.record(&rate(Dex::Lighter, Symbol::Btc, same, BASE_MS + 300_000))
            .unwrap();
        assert_eq!(rec.rows_written(), 2);
        assert_eq!(rec.rows_skipped(), 1);
    }

    #[test]
    fn change_detection_is_per_dex_and_symbol() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(dir.path(), 300);
        let same = dec!(0.0000125);

        rec.record(&rate(Dex::Lighter, Symbol::Btc, same, BASE_MS))
            .unwrap();
        // DEX が違えば独立に判定される
        rec.record(&rate(Dex::Hyperliquid, Symbol::Btc, same, BASE_MS))
            .unwrap();
        // 銘柄が違っても独立
        rec.record(&rate(Dex::Lighter, Symbol::Eth, same, BASE_MS))
            .unwrap();
        // 同じ (DEX, 銘柄) の再送だけがスキップされる
        rec.record(&rate(Dex::Lighter, Symbol::Btc, same, BASE_MS + 10))
            .unwrap();

        assert_eq!(rec.rows_written(), 3);
        assert_eq!(rec.rows_skipped(), 1);
    }

    #[test]
    fn splits_files_per_symbol_and_per_day() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(dir.path(), 300);
        rec.record(&rate(Dex::Lighter, Symbol::Btc, dec!(0.0001), BASE_MS))
            .unwrap();
        rec.record(&rate(Dex::Lighter, Symbol::Eth, dec!(0.0001), BASE_MS))
            .unwrap();
        rec.record(&rate(
            Dex::Lighter,
            Symbol::Btc,
            dec!(0.0002),
            BASE_MS + 86_400_000,
        ))
        .unwrap();
        rec.flush().unwrap();

        assert!(dir.path().join("2023-11-14_BTC_funding.csv").exists());
        assert!(dir.path().join("2023-11-14_ETH_funding.csv").exists());
        assert!(dir.path().join("2023-11-15_BTC_funding.csv").exists());
        assert_eq!(rec.writers.len(), 1, "前日分のハンドルは閉じられている");
    }

    #[test]
    fn annualized_column_normalizes_different_intervals() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(dir.path(), 300);

        // 1 時間 0.001% と 8 時間 0.008% は同じ年率になる
        let hourly = rate(Dex::Lighter, Symbol::Sol, dec!(0.00001), BASE_MS);
        let eight_hourly = FundingRate {
            current_rate: dec!(0.00008),
            interval_hours: Some(dec!(8)),
            ..rate(Dex::Aster, Symbol::Sol, dec!(0.00008), BASE_MS)
        };
        rec.record(&hourly).unwrap();
        rec.record(&eight_hourly).unwrap();
        rec.flush().unwrap();

        let rows = read_csv(&dir.path().join("2023-11-14_SOL_funding.csv"));
        assert_eq!(column(&rows[1], "annualized_pct"), "8.76");
        assert_eq!(column(&rows[2], "annualized_pct"), "8.76");
        // 1 精算あたりのレートは 8 倍違う。ここを直接比較してはいけない
        assert_ne!(
            column(&rows[1], "current_rate"),
            column(&rows[2], "current_rate")
        );
        assert_eq!(column(&rows[1], "interval_hours"), "1");
        assert_eq!(column(&rows[2], "interval_hours"), "8");
    }

    #[test]
    fn unknown_interval_leaves_annualized_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(dir.path(), 300);
        let unknown = FundingRate {
            interval_hours: None,
            predicted_rate: None,
            ..rate(Dex::EdgeX, Symbol::Hype, dec!(0.0001), BASE_MS)
        };
        rec.record(&unknown).unwrap();
        rec.flush().unwrap();

        let rows = read_csv(&dir.path().join("2023-11-14_HYPE_funding.csv"));
        assert_eq!(column(&rows[1], "interval_hours"), "");
        assert_eq!(column(&rows[1], "annualized_pct"), "");
        assert_eq!(column(&rows[1], "predicted_rate"), "");
        // 取得できた列は埋まっている
        assert_eq!(column(&rows[1], "current_rate"), "0.0001");
        assert_eq!(column(&rows[1], "latency_ms"), "12");
        assert_eq!(column(&rows[1], "index_price"), "36000");
    }

    #[test]
    fn liquidity_metrics_ride_along_on_the_same_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(dir.path(), 300);
        let with_stats = FundingRate {
            open_interest: Some(Quantity(dec!(1000))),
            volume_24h_usd: Some(dec!(72000000)),
            mark_price: Some(Price(dec!(36000))),
            ..rate(Dex::Hyperliquid, Symbol::Eth, dec!(0.0001), BASE_MS)
        };
        rec.record(&with_stats).unwrap();
        rec.flush().unwrap();

        let rows = read_csv(&dir.path().join("2023-11-14_ETH_funding.csv"));
        assert_eq!(column(&rows[1], "open_interest"), "1000");
        assert_eq!(column(&rows[1], "volume_24h_usd"), "72000000");
        // 72,000,000 / (1000 × 36,000) = 2
        assert_eq!(column(&rows[1], "volume_oi_ratio"), "2");
    }

    #[test]
    fn ratio_is_empty_when_units_cannot_be_normalized() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(dir.path(), 300);
        // 価格が無ければ OI を USD 換算できない → 比率は空欄（数量 ÷ USD を残さない）
        let no_price = FundingRate {
            open_interest: Some(Quantity(dec!(1000))),
            volume_24h_usd: Some(dec!(72000000)),
            mark_price: None,
            index_price: None,
            ..rate(Dex::Lighter, Symbol::Sol, dec!(0.0001), BASE_MS)
        };
        rec.record(&no_price).unwrap();
        rec.flush().unwrap();

        let rows = read_csv(&dir.path().join("2023-11-14_SOL_funding.csv"));
        assert_eq!(column(&rows[1], "open_interest"), "1000");
        assert_eq!(column(&rows[1], "volume_oi_ratio"), "");
    }

    #[test]
    fn missing_liquidity_metrics_leave_empty_columns() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(dir.path(), 300);
        // 取れない DEX があっても記録は続く（穴は空欄で残す）
        rec.record(&rate(Dex::EdgeX, Symbol::Hype, dec!(0.0001), BASE_MS))
            .unwrap();
        rec.flush().unwrap();

        let rows = read_csv(&dir.path().join("2023-11-14_HYPE_funding.csv"));
        assert_eq!(column(&rows[1], "open_interest"), "");
        assert_eq!(column(&rows[1], "volume_24h_usd"), "");
        assert_eq!(column(&rows[1], "volume_oi_ratio"), "");
    }

    #[test]
    fn appends_without_duplicating_header_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut rec = recorder_in(dir.path(), 300);
            rec.record(&rate(Dex::Lighter, Symbol::Btc, dec!(0.0001), BASE_MS))
                .unwrap();
            rec.flush().unwrap();
        }
        {
            // 再起動相当。同じファイルに追記され、ヘッダは重複しない
            let mut rec = recorder_in(dir.path(), 300);
            rec.record(&rate(Dex::Lighter, Symbol::Btc, dec!(0.0002), BASE_MS + 5))
                .unwrap();
            rec.flush().unwrap();
        }

        let rows = read_csv(&dir.path().join("2023-11-14_BTC_funding.csv"));
        assert_eq!(rows.len(), 3, "header + 2 rows: {rows:?}");
        assert_eq!(rows[0], FUNDING_CSV_HEADER.to_vec());
    }
}
