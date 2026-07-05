//! Shared backtest entry point — used by BOTH the CLI (`src/bin/backtest.rs`) and the
//! feature-gated Control Tower backtest API (`src/api/backtest_api.rs`), so the two
//! never drift. Owns the CLI-style timestamp parser and a file-free run pipeline that
//! collects the report/equity/trades payload the API stores in its run registry.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;

use super::bridge::dec_to_f64;
use super::harness::{run_backtest, BacktestConfig, BacktestOutcome};
use super::report::{build_report_json, run_rs_backtester};

/// One equity-curve sample (string-encoded Decimal, mirroring the rest of the API).
#[derive(Debug, Clone, Serialize)]
pub struct EquityPoint {
    pub ts: String,
    pub equity: String,
}

/// One closed trade row (string-encoded Decimals; mirrors the trades.csv columns).
#[derive(Debug, Clone, Serialize)]
pub struct TradeRecord {
    pub strategy: String,
    pub side: String,
    pub kind: String,
    pub entry_ts: String,
    pub exit_ts: String,
    pub entry_price: String,
    pub exit_price: String,
    pub shares: String,
    pub pnl: String,
    pub reason: String,
}

/// The full collected result of one backtest run (report JSON + equity + trades).
pub struct CollectedRun {
    pub report: serde_json::Value,
    pub equity: Vec<EquityPoint>,
    pub trades: Vec<TradeRecord>,
}

/// Run the harness + rs-backtester directional proxy for `cfg` and collect the
/// report/equity/trades payload WITHOUT writing any files (the CLI writes its own
/// via `report::write_native`; the API keeps everything in its in-memory registry).
pub async fn run_and_collect(cfg: &BacktestConfig) -> Result<CollectedRun> {
    let outcome = run_backtest(cfg).await?;
    let rs = run_rs_backtester(
        &cfg.coin,
        &outcome.candles,
        &outcome.stances,
        cfg.commission,
        dec_to_f64(cfg.starting_collateral),
    );
    let report = build_report_json(cfg, &outcome, &rs);
    let equity = collect_equity(&outcome);
    let trades = collect_trades(&outcome);
    Ok(CollectedRun { report, equity, trades })
}

fn collect_equity(outcome: &BacktestOutcome) -> Vec<EquityPoint> {
    outcome
        .ledger
        .equity_curve()
        .iter()
        .map(|(ts, eq)| EquityPoint {
            ts: ts.to_rfc3339(),
            equity: eq.to_string(),
        })
        .collect()
}

fn collect_trades(outcome: &BacktestOutcome) -> Vec<TradeRecord> {
    outcome
        .ledger
        .closed_trades()
        .iter()
        .map(|t| TradeRecord {
            strategy: t.strategy.clone(),
            side: t.side.clone(),
            kind: format!("{:?}", t.kind),
            entry_ts: t.entry_ts.to_rfc3339(),
            exit_ts: t.exit_ts.to_rfc3339(),
            entry_price: t.entry_price.to_string(),
            exit_price: t.exit_price.to_string(),
            shares: t.shares.to_string(),
            pnl: t.pnl.to_string(),
            reason: t.reason.clone(),
        })
        .collect()
}

/// Parse a timestamp: rfc3339, unix seconds (≤11 digits) or millis (≥12), or a
/// relative `now` / `now-<N>[smhd]` expression. Returns unix milliseconds.
///
/// Shared by the CLI arg parser and the API run form so both accept identical inputs.
pub fn parse_time(s: &str) -> Result<i64> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("now") {
        let now = Utc::now().timestamp_millis();
        if rest.is_empty() {
            return Ok(now);
        }
        // Expect "-<N><unit>" or "+<N><unit>".
        let sign = if rest.starts_with('-') { -1 } else { 1 };
        let body = rest.trim_start_matches(['-', '+']);
        let (num, unit) = body.split_at(body.len().saturating_sub(1));
        let n: i64 = num.parse().with_context(|| format!("bad relative time '{s}'"))?;
        let mult_ms = match unit {
            "s" => 1_000,
            "m" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            _ => bail!("unknown time unit in '{s}' (use s/m/h/d)"),
        };
        return Ok(now + sign * n * mult_ms);
    }
    if s.chars().all(|c| c.is_ascii_digit()) {
        let v: i64 = s.parse().context("parsing numeric timestamp")?;
        // ≤ 11 digits → seconds; otherwise milliseconds.
        return Ok(if s.len() <= 11 { v * 1000 } else { v });
    }
    let dt = DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("parsing rfc3339 timestamp '{s}'"))?;
    Ok(dt.with_timezone(&Utc).timestamp_millis())
}
