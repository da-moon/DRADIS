//! Backtesting subsystem — historical replay of REAL viper strategies.
//!
//! # What this is
//!
//! A feature-gated (`--features backtest`) offline harness that replays historical
//! Hyperliquid 1-minute candles + funding through the SAME `Strategy` trait objects
//! the live bot runs (`StrategyRegistry::create_all_strategies()`), behind the W1
//! clock seam (`ctx.wall_now` / `ctx.mono_now`) so every warmup / staleness /
//! cooldown / hold-time gate evaluates against HISTORICAL time at any replay speed.
//!
//! Two PnL views are produced, deliberately labelled:
//!   1. **Native Decimal ledger** (authoritative) — prices the actual binary YES/NO
//!      shares, settles 0/1 at expiry vs strike. See [`ledger`].
//!   2. **rs-backtester metrics** (directional proxy) — Sharpe / drawdown / win-rate
//!      on a `BUY/SHORTSELL/NULL` directional mapping of the underlying candle series.
//!      `Order{BUY,SHORTSELL,NULL}` models a linear instrument, not a binary option.
//!      See [`bridge`] + [`report`].
//!
//! # Honest fidelity tiers (published with every result)
//!
//! * **Tier A (high):** drift-gated directional logic — `drift_10m` / `drift_60m`
//!   (60/10 one-minute samples — faithful) and strike-distance gates (e.g.
//!   TrendReversal's drift/exhaustion entries).
//! * **Tier B (medium):** velocity gates — `velocity_5s`/`velocity_1s` are derived
//!   from 1-minute closes at 60s synthetic steps, so the sub-5s windows are
//!   IDENTICALLY 0 (HL has no sub-5s candles). MOMENTUM is therefore effectively
//!   EXCLUDED at 1m: every one of its entry branches requires `velocity ≠ 0`, so it
//!   can never fire — its "0 trades" is a harness limitation, not a strategy verdict.
//!   Funding is a real historical series.
//! * **Tier C (model-dependent):** the 8 Polymarket book fields are a parametric
//!   binary-option model (configurable spread/depth), and `institutional_pulse`,
//!   `tide_coherence`, `oi_delta_pct`, `cvd_ratio` have no historical source and are
//!   0. Convergence therefore no-ops by design (pulse=0 → NoSignal), and
//!   TrendReversal's SQLite cascade guard no-ops without a live pool.
//!
//! `hyperliquid-backtest` is deliberately NOT a dependency: its advertised
//! fetch/backtest/report API is unreachable dead code in the published crate.
//! Funding/candles are fetched directly from `https://api.hyperliquid.xyz/info`.

pub mod fetch;
pub mod clock;
pub mod synth;
pub mod ledger;
pub mod harness;
pub mod bridge;
pub mod report;
pub mod llm_score;
pub mod entry;

pub use clock::ReplayClock;
pub use fetch::{BacktestCache, Candle, FundingPoint};
pub use harness::{BacktestConfig, run_backtest};
pub use synth::{book_model, normalize_funding_8h, BookQuote, SnapshotSynthesizer};

/// Fidelity-tier disclaimer block, printed verbatim by the CLI and mirrored in the
/// README. Kept in one place so the two never drift.
pub const FIDELITY_DISCLAIMER: &str = "\
── Fidelity tiers (read before trusting these numbers) ─────────────────────────
 Tier A (high)    drift_10m/drift_60m gates, strike-distance gates — faithful.
 Tier B (medium)  velocity gates: velocity_5s/1s come from 1m closes at 60s steps,
                  so sub-5s windows are IDENTICALLY 0 (HL has no sub-5s candles).
                  MOMENTUM is therefore EXCLUDED at 1m — every entry branch needs
                  velocity != 0, so it can never fire (its 0 trades is a harness
                  limitation, not a verdict). Funding is a real historical series.
 Tier C (model)   The 8 Polymarket book fields are a parametric binary-option model
                  (configurable --spread/--depth), NOT a real order book. OI/CVD and
                  institutional_pulse/tide_coherence have no historical source (=0),
                  so CONVERGENCE is EXCLUDED (always no-ops), and TrendReversal's
                  SQLite cascade guard no-ops. Funding is signal-only (binary shares
                  pay no carry). The rs-backtester Sharpe/drawdown/win-rate are a
                  DIRECTIONAL PROXY on the underlying, not the binary payoff.
────────────────────────────────────────────────────────────────────────────────";
