//! ネットデルタの CSV 出力。
//!
//! **ファイル**: `data/{YYYY-MM-DD}_net_delta.csv`（銘柄で分けない。1 行 = 1 銘柄の
//! 照合結果で、更新頻度が低く行数も少ないため、1 ファイルの方が突き合わせやすい）
//!
//! フェーズ2 以降で「**どれくらいデルタがずれるものなのか**」を実測するための
//! 記録。閾値（`warn` / `rebalance`）を最終的に決めるのはこのデータになる。
//!
//! 価格差 CSV / ファンディング CSV と違い、**変化検知は行わない**。照合した
//! 周期はすべて残す（ずれていない時間の長さも分析対象になるため）。

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::time::Duration;

use chrono::NaiveDate;
use config::RecordingConfig;
use core_types::Dex;
use risk::NetDeltaStatus;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::csv_writer::{fmt_opt, open_writer_with_header, wall_ms_to_date, CsvError};

/// USD 換算値の小数桁数。
const USD_SCALE: u32 = 4;

/// ネットデルタ CSV のカラム定義。
///
/// `pos_*` は [`Dex::ALL`] の宣言順に並ぶ。DEX を追加してもヘッダと行の順序が
/// ずれないよう、どちらも同じ配列から作る。
pub fn net_delta_csv_header() -> Vec<String> {
    let mut header = vec!["timestamp_ms".to_string(), "symbol".to_string()];
    header.extend(Dex::ALL.iter().map(|d| format!("pos_{d}")));
    header.extend(
        [
            // 数量ベースのネットポジション
            "net_delta",
            // ノーショナル換算（**判定はこちらで行う**）
            "net_delta_usd",
            // 換算に使った価格。空欄なら判定は保留されている
            "mark_price",
            // 想定ポジションとの乖離（USD, 絶対値の総和）
            "drift_usd",
            // none / warned / rebalanced / halted_soft / halted / price_unavailable
            "action",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    header
}

/// 日次でファイルを分けてネットデルタの状態を書き出す。
pub struct NetDeltaRecorder {
    dir: PathBuf,
    writer: Option<(NaiveDate, csv::Writer<BufWriter<File>>)>,
    rows_written: u64,
}

impl NetDeltaRecorder {
    pub fn new(cfg: &RecordingConfig) -> Self {
        NetDeltaRecorder {
            dir: cfg.csv_dir.clone(),
            writer: None,
            rows_written: 0,
        }
    }

    pub fn rows_written(&self) -> u64 {
        self.rows_written
    }

    pub fn record(&mut self, status: &NetDeltaStatus) -> Result<(), CsvError> {
        let date = wall_ms_to_date(status.checked_at_wall_ms);
        let writer = self.writer_for(date)?;
        writer.write_record(status_to_row(status))?;
        self.rows_written += 1;
        Ok(())
    }

    fn writer_for(
        &mut self,
        date: NaiveDate,
    ) -> Result<&mut csv::Writer<BufWriter<File>>, CsvError> {
        // 日付が変わったら前日のハンドルは閉じる（flush してから drop）。
        if self.writer.as_ref().is_some_and(|(d, _)| *d != date) {
            self.flush()?;
            self.writer = None;
        }
        if self.writer.is_none() {
            let path = self
                .dir
                .join(format!("{}_net_delta.csv", date.format("%Y-%m-%d")));
            let writer = open_writer_with_header(&path, &net_delta_csv_header())?;
            info!(file = %path.display(), "ネットデルタ CSV を開きました");
            self.writer = Some((date, writer));
        }
        Ok(&mut self.writer.as_mut().expect("直前に挿入済み").1)
    }

    pub fn flush(&mut self) -> Result<(), CsvError> {
        if let Some((date, writer)) = self.writer.as_mut() {
            if let Err(e) = writer.flush() {
                error!(error = %e, date = %date, "ネットデルタ CSV の flush に失敗");
                return Err(CsvError::Io(e));
            }
        }
        Ok(())
    }
}

impl Drop for NetDeltaRecorder {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// 状態を [`net_delta_csv_header`] と同じ順序の 1 行に変換する。
pub fn status_to_row(status: &NetDeltaStatus) -> Vec<String> {
    let mut row = vec![
        status.checked_at_wall_ms.to_string(),
        status.symbol.to_string(),
    ];
    // 照合できなかった DEX は空欄。**0 とは区別する**
    //（「フラット」と「取得できていない」を混同しない）。
    row.extend(
        Dex::ALL
            .iter()
            .map(|d| fmt_opt(status.positions.get(d).map(|q| q.0.normalize().to_string()))),
    );
    row.push(status.net_delta.0.normalize().to_string());
    row.push(fmt_opt(
        status
            .net_delta_usd
            .map(|v| v.round_dp(USD_SCALE).normalize().to_string()),
    ));
    row.push(fmt_opt(
        status.mark_price.map(|p| p.0.normalize().to_string()),
    ));
    row.push(fmt_opt(
        status
            .drift_usd
            .map(|v| v.round_dp(USD_SCALE).normalize().to_string()),
    ));
    row.push(status.action.as_str().to_string());
    row
}

/// ネットデルタ CSV の書き込みタスクを起動する。
///
/// **監視タスクとは独立させる。** ここが詰まっても監視は止まらない
/// （送信側が `try_send` で捨てる）。
pub fn spawn_net_delta_csv_writer(
    cfg: &RecordingConfig,
    mut rx: mpsc::Receiver<NetDeltaStatus>,
) -> JoinHandle<()> {
    let mut recorder = NetDeltaRecorder::new(cfg);
    let flush_interval = Duration::from_millis(cfg.csv_flush_interval_ms.max(1));

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(flush_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut write_errors: u64 = 0;

        loop {
            tokio::select! {
                maybe_status = rx.recv() => {
                    match maybe_status {
                        Some(status) => {
                            if let Err(e) = recorder.record(&status) {
                                write_errors += 1;
                                if write_errors == 1 || write_errors % 100 == 0 {
                                    error!(error = %e, count = write_errors, "ネットデルタ CSV 書き込みエラー");
                                }
                            }
                        }
                        None => break,
                    }
                }
                _ = ticker.tick() => {
                    if let Err(e) = recorder.flush() {
                        warn!(error = %e, "ネットデルタ CSV の定期 flush に失敗");
                    }
                }
            }
        }

        if let Err(e) = recorder.flush() {
            error!(error = %e, "終了時のネットデルタ CSV flush に失敗");
        }
        info!(
            rows_written = recorder.rows_written(),
            write_errors, "ネットデルタ CSV writer タスク終了"
        );
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{Price, Quantity, Symbol};
    use risk::{NetDeltaAction, PositionManager};
    use rust_decimal_macros::dec;
    use std::path::Path;

    const BASE_MS: u64 = 1_700_000_000_000;

    fn recorder_in(dir: &Path) -> NetDeltaRecorder {
        let cfg = RecordingConfig {
            csv_dir: dir.to_path_buf(),
            ..RecordingConfig::default()
        };
        NetDeltaRecorder::new(&cfg)
    }

    fn status(hl: rust_decimal::Decimal, lighter: rust_decimal::Decimal) -> NetDeltaStatus {
        let mut manager = PositionManager::new(config::NetDeltaConfig::default());
        manager
            .expected_mut()
            .set(Dex::Hyperliquid, Symbol::Btc, Quantity(hl));
        manager
            .expected_mut()
            .set(Dex::Lighter, Symbol::Btc, Quantity(lighter));
        manager.evaluate(
            Symbol::Btc,
            &[
                risk::DexPosition::new(Dex::Hyperliquid, Symbol::Btc, Quantity(hl)),
                risk::DexPosition::new(Dex::Lighter, Symbol::Btc, Quantity(lighter)),
            ],
            Some(Price(dec!(36000))),
            BASE_MS,
        )
    }

    fn read_csv(path: &Path) -> Vec<Vec<String>> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| l.split(',').map(|s| s.to_string()).collect())
            .collect()
    }

    fn column(row: &[String], name: &str) -> String {
        let idx = net_delta_csv_header()
            .iter()
            .position(|h| h == name)
            .unwrap();
        row[idx].clone()
    }

    #[test]
    fn row_matches_header_length() {
        assert_eq!(
            status_to_row(&status(dec!(0.5), dec!(-0.5))).len(),
            net_delta_csv_header().len()
        );
    }

    #[test]
    fn records_every_check_without_change_detection() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(dir.path());
        // 同じ状態でも毎回残す（ずれていない時間の長さも分析対象）
        for _ in 0..3 {
            rec.record(&status(dec!(0.5), dec!(-0.5))).unwrap();
        }
        rec.flush().unwrap();

        let rows = read_csv(&dir.path().join("2023-11-14_net_delta.csv"));
        assert_eq!(rows.len(), 4, "header + 3 rows: {rows:?}");
        assert_eq!(rows[0], net_delta_csv_header());
        assert_eq!(column(&rows[1], "net_delta"), "0");
        assert_eq!(column(&rows[1], "action"), "none");
    }

    #[test]
    fn writes_per_dex_positions_and_notional() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(dir.path());
        // +0.5 / -0.497 → net +0.003（108 USD）
        rec.record(&status(dec!(0.5), dec!(-0.497))).unwrap();
        rec.flush().unwrap();

        let rows = read_csv(&dir.path().join("2023-11-14_net_delta.csv"));
        assert_eq!(column(&rows[1], "pos_hyperliquid"), "0.5");
        assert_eq!(column(&rows[1], "pos_lighter"), "-0.497");
        // 照合対象外の DEX は空欄（0 とは区別する）
        assert_eq!(column(&rows[1], "pos_edgex"), "");
        assert_eq!(column(&rows[1], "net_delta"), "0.003");
        assert_eq!(column(&rows[1], "net_delta_usd"), "108");
        assert_eq!(column(&rows[1], "mark_price"), "36000");
        assert_eq!(column(&rows[1], "drift_usd"), "0");
        assert_eq!(column(&rows[1], "action"), NetDeltaAction::Warn.as_str());
    }

    #[test]
    fn splits_files_per_day() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = recorder_in(dir.path());
        rec.record(&status(dec!(0.5), dec!(-0.5))).unwrap();

        let mut next_day = status(dec!(0.5), dec!(-0.5));
        next_day.checked_at_wall_ms = BASE_MS + 86_400_000;
        rec.record(&next_day).unwrap();
        rec.flush().unwrap();

        assert!(dir.path().join("2023-11-14_net_delta.csv").exists());
        assert!(dir.path().join("2023-11-15_net_delta.csv").exists());
    }
}
