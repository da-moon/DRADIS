//! W8 — `backtest` CLI (feature-gated; `required-features = ["backtest"]`).
//!
//! Replays historical market data (Hyperliquid or Binance FAPI, via `--source`) through
//! the REAL viper strategies and prints a per-strategy summary + native Decimal PnL +
//! rs-backtester directional-proxy metrics + an honest fidelity-tier disclaimer.
//!
//! Usage:
//!   cargo run --features backtest --bin backtest -- \
//!     --coin BTC --start <rfc3339|unix|now-6h> --end <…|now-1h> \
//!     [--interval 1m] [--strategies momentum,trendreversal] \
//!     [--spread 0.02] [--depth 500] [--commission 0.0] [--starting 500] \
//!     [--llm-score] [--cache backtest_cache.sqlite] [--out backtest_out] \
//!     [--source hyperliquid|binance]

use std::str::FromStr;

use anyhow::{bail, Result};
use clap::Parser;
use rust_decimal::Decimal;

use dradis::backtest::entry::parse_time;
use dradis::backtest::harness::{run_backtest, BacktestConfig};
use dradis::backtest::report::{print_summary, run_rs_backtester, write_native};
use dradis::backtest::source::SourceKind;

/// DRADIS backtest — replay historical market data through the real vipers.
#[derive(Parser, Debug)]
#[command(name = "backtest", version, about)]
struct Cli {
    /// Coin symbol, e.g. BTC (DRADIS asset — mapped to the provider's wire symbol per --source)
    #[arg(long)]
    coin: String,

    /// rfc3339 | unix(s|ms) | now-6h
    #[arg(long)]
    start: String,

    /// rfc3339 | unix(s|ms) | now-1h
    #[arg(long)]
    end: String,

    /// Candle interval (default 1m)
    #[arg(long, default_value = "1m")]
    interval: String,

    /// Subset by short name (momentum,trendreversal,gboost,basis,maker,timedecay,
    /// arbitrage,convergence); comma-separated; default = all
    #[arg(long, value_delimiter = ',')]
    strategies: Option<Vec<String>>,

    /// Book-model half-spread (Tier C)
    #[arg(long, default_value = "0.02", value_parser = parse_decimal, allow_negative_numbers = true)]
    spread: Decimal,

    /// Modeled depth, shares/side (Tier C)
    #[arg(long, default_value = "500", value_parser = parse_decimal, allow_negative_numbers = true)]
    depth: Decimal,

    /// Fee rate (native ledger + rs-backtester proxy)
    #[arg(long, default_value = "0", value_parser = parse_decimal, allow_negative_numbers = true)]
    commission: Decimal,

    /// Starting collateral ($)
    #[arg(long, default_value = "500", value_parser = parse_decimal, allow_negative_numbers = true)]
    starting: Decimal,

    /// Experimental: LLM conviction scoring (needs a provider)
    #[arg(long)]
    llm_score: bool,

    /// Backtest SQLite cache path
    #[arg(long, default_value = "backtest_cache.sqlite")]
    cache: String,

    /// Output dir for report.json/trades.csv/equity.csv
    #[arg(long, default_value = "backtest_out")]
    out: String,

    /// Historical-data provider (both hit public, unauthenticated endpoints — no key needed)
    #[arg(long, value_enum, default_value_t = SourceKind::Hyperliquid)]
    source: SourceKind,
}

fn parse_decimal(s: &str) -> Result<Decimal, String> {
    Decimal::from_str(s).map_err(|e| e.to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let coin = cli.coin.to_uppercase();
    let start_ms = parse_time(&cli.start)?;
    let end_ms = parse_time(&cli.end)?;
    if end_ms <= start_ms {
        bail!("--end ({end_ms}) must be after --start ({start_ms})");
    }
    // NOTE: an effectively-empty `--strategies ""` stays `Some(vec![])` — "run
    // NOTHING" — exactly like the pre-clap parser (configure_enables disables all
    // strategies for an empty subset). Do not collapse it to `None`, which would
    // silently invert it into "run everything".
    let strategies = cli.strategies.map(|v| {
        v.into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
    });

    let cfg = BacktestConfig {
        crypto_filter: coin.to_lowercase(),
        coin,
        interval: cli.interval,
        start_ms,
        end_ms,
        strategies,
        half_spread: cli.spread,
        depth: cli.depth,
        commission: cli.commission,
        starting_collateral: cli.starting,
        llm_score: cli.llm_score,
        cache_path: cli.cache,
        out_dir: cli.out,
        sigma_window: 60,
        source: cli.source,
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
