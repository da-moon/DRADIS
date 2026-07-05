//! W8 — `backtest` CLI (feature-gated; `required-features = ["backtest"]`).
//!
//! Replays historical Hyperliquid data through the REAL viper strategies and prints a
//! per-strategy summary + native Decimal PnL + rs-backtester directional-proxy metrics
//! + an honest fidelity-tier disclaimer.
//!
//! Usage:
//!   cargo run --features backtest --bin backtest -- \
//!     --coin BTC --start <rfc3339|unix|now-6h> --end <…|now-1h> \
//!     [--interval 1m] [--strategies momentum,trendreversal] \
//!     [--spread 0.02] [--depth 500] [--commission 0.0] \
//!     [--llm-score] [--cache backtest_cache.sqlite] [--out backtest_out]

use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use rust_decimal::Decimal;

use dradis::backtest::entry::parse_time;
use dradis::backtest::harness::{run_backtest, BacktestConfig};
use dradis::backtest::report::{print_summary, run_rs_backtester, write_native};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_usage();
        return Ok(());
    }

    let mut coin: Option<String> = None;
    let mut start: Option<String> = None;
    let mut end: Option<String> = None;
    let mut interval = "1m".to_string();
    let mut strategies: Option<Vec<String>> = None;
    let mut spread = Decimal::from_str("0.02").unwrap();
    let mut depth = Decimal::from_str("500").unwrap();
    let mut commission = Decimal::ZERO;
    let mut llm_score = false;
    let mut cache = "backtest_cache.sqlite".to_string();
    let mut out = "backtest_out".to_string();
    let mut starting = Decimal::from_str("500").unwrap();

    let mut i = 0;
    while i < args.len() {
        let a = args[i].clone();
        match a.as_str() {
            "--coin" => coin = Some(val(&args, &mut i, "--coin")?),
            "--start" => start = Some(val(&args, &mut i, "--start")?),
            "--end" => end = Some(val(&args, &mut i, "--end")?),
            "--interval" => interval = val(&args, &mut i, "--interval")?,
            "--strategies" => {
                strategies = Some(
                    val(&args, &mut i, "--strategies")?
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                )
            }
            "--spread" => spread = Decimal::from_str(&val(&args, &mut i, "--spread")?).context("--spread")?,
            "--depth" => depth = Decimal::from_str(&val(&args, &mut i, "--depth")?).context("--depth")?,
            "--commission" => {
                commission = Decimal::from_str(&val(&args, &mut i, "--commission")?).context("--commission")?
            }
            "--starting" => {
                starting = Decimal::from_str(&val(&args, &mut i, "--starting")?).context("--starting")?
            }
            "--llm-score" => llm_score = true,
            "--cache" => cache = val(&args, &mut i, "--cache")?,
            "--out" => out = val(&args, &mut i, "--out")?,
            other => bail!("unknown argument: {other} (try --help)"),
        }
        i += 1;
    }

    let coin = coin.ok_or_else(|| anyhow!("--coin is required (e.g. --coin BTC)"))?.to_uppercase();
    let start_ms = parse_time(&start.ok_or_else(|| anyhow!("--start is required"))?)?;
    let end_ms = parse_time(&end.ok_or_else(|| anyhow!("--end is required"))?)?;
    if end_ms <= start_ms {
        bail!("--end ({end_ms}) must be after --start ({start_ms})");
    }

    let cfg = BacktestConfig {
        crypto_filter: coin.to_lowercase(),
        coin,
        interval,
        start_ms,
        end_ms,
        strategies,
        half_spread: spread,
        depth,
        commission,
        starting_collateral: starting,
        llm_score,
        cache_path: cache,
        out_dir: out,
        sigma_window: 60,
    };

    let outcome = run_backtest(&cfg).await?;
    let rs = run_rs_backtester(
        &cfg.coin,
        &outcome.candles,
        &outcome.stances,
        cfg.commission,
        dradis::backtest::bridge::dec_to_f64(cfg.starting_collateral),
    );
    write_native(&cfg, &outcome, &rs)?;
    print_summary(&cfg, &outcome, &rs);
    Ok(())
}

/// Consume and return the value following a flag at `args[*i]`, advancing `*i`.
fn val(args: &[String], i: &mut usize, flag: &str) -> Result<String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| anyhow!("missing value for {flag}"))
}

fn print_usage() {
    println!(
        "DRADIS backtest — replay historical Hyperliquid data through the real vipers.\n\
\n\
REQUIRED:\n\
  --coin BTC              Hyperliquid coin symbol\n\
  --start <t>             rfc3339 | unix(s|ms) | now-6h\n\
  --end <t>               rfc3339 | unix(s|ms) | now-1h\n\
\n\
OPTIONAL:\n\
  --interval 1m           candle interval (default 1m)\n\
  --strategies a,b        subset by short name (momentum,trendreversal,gboost,basis,\n\
                          maker,timedecay,arbitrage,convergence); default = all\n\
  --spread 0.02           book-model half-spread (Tier C)\n\
  --depth 500             modeled depth, shares/side (Tier C)\n\
  --commission 0.0        fee rate (native ledger + rs-backtester proxy)\n\
  --starting 500          starting collateral ($)\n\
  --llm-score             experimental: LLM conviction scoring (needs a provider)\n\
  --cache <path>          backtest SQLite cache (default backtest_cache.sqlite)\n\
  --out <dir>             output dir for report.json/trades.csv/equity.csv\n"
    );
}
