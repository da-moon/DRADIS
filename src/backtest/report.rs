//! W6 — Reporting: rs-backtester metrics (directional proxy) + native Decimal output.
//!
//! Emits two deliberately-labelled PnL views:
//!   1. the **native Decimal ledger** (authoritative binary settlement), and
//!   2. **rs-backtester** Sharpe/drawdown/win-rate on the directional proxy.
//!
//! rs-backtester's commission is a PROCESS-GLOBAL singleton: `update_config` is called
//! immediately before `Backtest::new`, single-threaded, so no concurrent construction
//! can race the global `RwLock`. `plot()`/`i_chart()` are NEVER called (they hard-code
//! a Windows `"\\"` path join that corrupts output on Linux) — we only read
//! `Backtest.metrics` and print `report_horizontal`.

use std::fs;
use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::DateTime;
use rust_decimal::Decimal;

use rs_backtester::backtester::Backtest;
use rs_backtester::config::update_config;
use rs_backtester::data::Data;
use rs_backtester::metrics::report_horizontal;
use rs_backtester::strategies::Strategy as RsStrategy;

use super::bridge::{dec_to_f64, dec_to_u64, to_orders, Stance};
use super::fetch::Candle;
use super::harness::{BacktestConfig, BacktestOutcome};
use super::FIDELITY_DISCLAIMER;

/// Extracted rs-backtester metrics (kept as a plain struct so rs types don't leak).
#[derive(Debug, Clone, Default)]
pub struct RsMetrics {
    pub bt_return_pct: Option<f64>,
    pub sharpe: Option<f64>,
    pub max_drawdown_pct: Option<f64>,
    pub win_rate_pct: Option<f64>,
    pub trades_nr: Option<usize>,
}

/// Build an rs-backtester `Data` from the candle series (the single f64/u64 boundary).
fn build_data(coin: &str, candles: &[Candle]) -> Data {
    let mut datetime = Vec::with_capacity(candles.len());
    let mut open = Vec::with_capacity(candles.len());
    let mut high = Vec::with_capacity(candles.len());
    let mut low = Vec::with_capacity(candles.len());
    let mut close = Vec::with_capacity(candles.len());
    let mut volume = Vec::with_capacity(candles.len());
    for c in candles {
        let dt = DateTime::from_timestamp_millis(c.ts_ms)
            .unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap())
            .fixed_offset();
        datetime.push(dt);
        open.push(dec_to_f64(c.open));
        high.push(dec_to_f64(c.high));
        low.push(dec_to_f64(c.low));
        close.push(dec_to_f64(c.close));
        volume.push(dec_to_u64(c.volume));
    }
    Data { ticker: coin.to_string(), datetime, open, high, low, close, volume }
}

/// Run the rs-backtester directional proxy and return its metrics. Returns `None`
/// (and logs) if the series is too short or the crate panics on the data.
pub fn run_rs_backtester(
    coin: &str,
    candles: &[Candle],
    stances: &[Stance],
    commission: Decimal,
    initial: f64,
) -> Option<RsMetrics> {
    if candles.len() < 2 || candles.len() != stances.len() {
        tracing::warn!(
            "rs-backtester skipped: need ≥2 aligned candles/stances (got {} / {})",
            candles.len(),
            stances.len()
        );
        return None;
    }

    // Global config singleton — set BEFORE Backtest::new, single-threaded.
    // Also swap the default `AllInSizerWholeUnits` (which sizes in WHOLE units of the
    // underlying and so truncates to 0 for any coin priced above the starting
    // collateral — BTC/ETH — yielding a constant networth, 0% return and NaN sharpe)
    // for the FRACTIONAL `AllInSizer`, so the directional proxy actually trades.
    let rate = dec_to_f64(commission);
    update_config(move |cfg| {
        cfg.commission_rate = rate;
        cfg.sizer = Box::new(rs_backtester::risk_manager::AllInSizer);
    });

    let data = Arc::new(build_data(coin, candles));
    let strategy = RsStrategy {
        name: format!("{coin} directional proxy"),
        choices: to_orders(stances),
        indicator: None,
        data,
    };

    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let bt = Backtest::new(strategy, initial);
        report_horizontal(&[&bt]);
        let m = &bt.metrics;
        RsMetrics {
            bt_return_pct: m.bt_return,
            sharpe: m.sharpe,
            max_drawdown_pct: m.max_drawd.map(|d| d * 100.0),
            win_rate_pct: m.win_rate.map(|w| w * 100.0),
            trades_nr: m.trades_nr,
        }
    }));

    match result {
        Ok(m) => Some(m),
        Err(_) => {
            tracing::warn!("rs-backtester panicked on this series — skipping proxy metrics");
            None
        }
    }
}

fn final_equity(outcome: &BacktestOutcome) -> Decimal {
    outcome
        .ledger
        .equity_curve()
        .last()
        .map(|(_, e)| *e)
        .unwrap_or_else(|| outcome.ledger.starting() + outcome.ledger.realized())
}

/// Write `report.json`, `trades.csv`, and `equity.csv` into `cfg.out_dir`.
pub fn write_native(cfg: &BacktestConfig, outcome: &BacktestOutcome, rs: &Option<RsMetrics>) -> Result<()> {
    let dir = Path::new(&cfg.out_dir);
    fs::create_dir_all(dir).with_context(|| format!("creating out dir {}", cfg.out_dir))?;

    // ── trades.csv ──────────────────────────────────────────────────────────
    let mut tf = fs::File::create(dir.join("trades.csv")).context("creating trades.csv")?;
    writeln!(tf, "strategy,side,kind,entry_ts,exit_ts,entry_price,exit_price,shares,pnl,reason")?;
    for t in outcome.ledger.closed_trades() {
        writeln!(
            tf,
            "{},{},{:?},{},{},{},{},{},{},{}",
            t.strategy,
            t.side,
            t.kind,
            t.entry_ts.to_rfc3339(),
            t.exit_ts.to_rfc3339(),
            t.entry_price,
            t.exit_price,
            t.shares,
            t.pnl,
            csv_escape(&t.reason),
        )?;
    }

    // ── equity.csv ──────────────────────────────────────────────────────────
    let mut ef = fs::File::create(dir.join("equity.csv")).context("creating equity.csv")?;
    writeln!(ef, "ts,equity")?;
    for (ts, eq) in outcome.ledger.equity_curve() {
        writeln!(ef, "{},{}", ts.to_rfc3339(), eq)?;
    }

    // ── report.json ─────────────────────────────────────────────────────────
    let report = build_report_json(cfg, outcome, rs);

    let mut rf = fs::File::create(dir.join("report.json")).context("creating report.json")?;
    rf.write_all(serde_json::to_string_pretty(&report)?.as_bytes())?;

    tracing::info!("📝 wrote report.json / trades.csv / equity.csv to {}", cfg.out_dir);
    Ok(())
}

/// Build the `report.json` document as a `serde_json::Value`. This is the single
/// source of truth for the report structure — `write_native` serializes it to disk
/// for the CLI, and the feature-gated Control Tower backtest API serves it verbatim.
pub fn build_report_json(
    cfg: &BacktestConfig,
    outcome: &BacktestOutcome,
    rs: &Option<RsMetrics>,
) -> serde_json::Value {
    let per_strategy: Vec<serde_json::Value> = outcome
        .ledger
        .per_strategy()
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "strategy": s.strategy,
                "trades": s.trades,
                "wins": s.wins,
                "win_rate_pct": (s.win_rate * 100.0),
                "pnl": s.pnl.to_string(),
            })
        })
        .collect();

    let rs_json = rs.as_ref().map(|m| {
        serde_json::json!({
            "note": "directional proxy on the underlying (BUY/SHORTSELL/NULL), NOT the binary payoff",
            "return_pct": m.bt_return_pct,
            "sharpe": m.sharpe,
            "max_drawdown_pct": m.max_drawdown_pct,
            "win_rate_pct": m.win_rate_pct,
            "trades_nr": m.trades_nr,
        })
    });

    let llm_json: Vec<serde_json::Value> = outcome
        .scored_entries
        .iter()
        .map(|s| {
            serde_json::json!({
                "strategy": s.strategy,
                "side": s.side,
                "entry_ts": s.entry_ts.to_rfc3339(),
                "score": s.score,
                "rationale": s.rationale,
                "realized_pnl": s.realized_pnl.map(|p| p.to_string()),
            })
        })
        .collect();

    serde_json::json!({
        "coin": cfg.coin,
        "interval": cfg.interval,
        "start_ms": cfg.start_ms,
        "end_ms": cfg.end_ms,
        // Requested start/end above are the CLI window; these are the range actually
        // replayed. They differ when Hyperliquid's ~5000-candle tail retention
        // head-truncates the request (a warning is also logged at run time).
        "replayed_start_ms": outcome.candles.first().map(|c| c.ts_ms),
        "replayed_end_ms": outcome.candles.last().map(|c| c.ts_ms),
        "ticks": outcome.ticks,
        "markets": outcome.markets,
        "params": {
            "spread": cfg.half_spread.to_string(),
            "depth": cfg.depth.to_string(),
            "commission": cfg.commission.to_string(),
            "strategies": cfg.strategies,
            "llm_score": cfg.llm_score,
        },
        "native_ledger": {
            "note": "AUTHORITATIVE — prices real binary YES/NO shares, settles 0/1 at expiry",
            "starting_collateral": outcome.ledger.starting().to_string(),
            "realized_pnl": outcome.ledger.realized().to_string(),
            "final_equity": final_equity(outcome).to_string(),
            "closed_trades": outcome.ledger.closed_trades().len(),
            "per_strategy": per_strategy,
        },
        "rs_backtester": rs_json,
        "llm_scores": llm_json,
        "fidelity": FIDELITY_DISCLAIMER,
    })
}

/// Human-readable stdout summary: per-strategy table, native PnL, proxy metrics, and
/// the fidelity-tier disclaimer block.
pub fn print_summary(cfg: &BacktestConfig, outcome: &BacktestOutcome, rs: &Option<RsMetrics>) {
    println!("\n════════════════════ DRADIS BACKTEST ════════════════════");
    println!(
        " {} {}  |  {} ticks over {} synthetic hourly markets",
        cfg.coin, cfg.interval, outcome.ticks, outcome.markets
    );
    println!(
        " spread=±{}  depth={}sh/side  commission={}",
        cfg.half_spread, cfg.depth, cfg.commission
    );
    println!("──────────────────────────────────────────────────────────");
    println!(
        " {:<16} {:>7} {:>7} {:>10}",
        "Strategy", "Trades", "Win%", "PnL($)"
    );
    let per = outcome.ledger.per_strategy();
    if per.is_empty() {
        println!("   (no trades fired — all vipers stood down over this window)");
    }
    for s in &per {
        println!(
            " {:<16} {:>7} {:>6.1}% {:>10}",
            s.strategy,
            s.trades,
            s.win_rate * 100.0,
            round2(s.pnl)
        );
    }
    println!("──────────────────────────────────────────────────────────");
    println!(" NATIVE LEDGER (authoritative binary settlement)");
    println!("   starting     : ${}", cfg.starting_collateral);
    println!("   realized PnL : ${}", round2(outcome.ledger.realized()));
    println!("   final equity : ${}", round2(final_equity(outcome)));
    println!("   closed trades: {}", outcome.ledger.closed_trades().len());
    match rs {
        Some(m) => {
            println!(" RS-BACKTESTER (directional proxy on the underlying)");
            println!(
                "   return={}  sharpe={}  maxDD={}  winRate={}  trades={}",
                fmt_opt_pct(m.bt_return_pct),
                fmt_opt(m.sharpe),
                fmt_opt_pct(m.max_drawdown_pct),
                fmt_opt_pct(m.win_rate_pct),
                m.trades_nr.map(|n| n.to_string()).unwrap_or_else(|| "-".into()),
            );
        }
        None => println!(" RS-BACKTESTER : (skipped — see log)"),
    }
    if !outcome.scored_entries.is_empty() {
        let n = outcome.scored_entries.len();
        let avg = outcome.scored_entries.iter().map(|s| s.score as f64).sum::<f64>() / n as f64;
        println!(" LLM SCORING  : {n} entries scored (avg conviction {avg:.0}/100) — see report.json");
    }
    println!("{FIDELITY_DISCLAIMER}");
    println!("══════════════════════════════════════════════════════════\n");
}

fn round2(d: Decimal) -> Decimal {
    d.round_dp(2)
}
fn fmt_opt(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.2}")).unwrap_or_else(|| "-".into())
}
fn fmt_opt_pct(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.2}%")).unwrap_or_else(|| "-".into())
}
fn csv_escape(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}
