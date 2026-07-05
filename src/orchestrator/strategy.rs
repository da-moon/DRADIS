/// Strategy trait: defines the interface all trading strategies must implement.
///
/// This trait allows strategies to be treated uniformly by the orchestrator,
/// enabling independent strategy threading later while maintaining a common interface.

use anyhow::Result;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use crate::state::{MarketConfig, MarketSnapshot, StrategySignal, StrategyStatus, PositionMap};
use crate::helpers::dynamic_config::DynamicConfig;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

/// Context passed to strategies containing all market data and shared state they need.
#[derive(Clone)]
pub struct StrategyContext {
    /// Current market configuration and metadata
    pub market: MarketConfig,
    /// Current market snapshot (prices, oracle, velocity)
    pub snapshot: MarketSnapshot,
    /// All open positions (shared, read-only for strategies)
    pub positions: Arc<Mutex<PositionMap>>,
    /// Total session PnL booked so far.
    pub session_pnl: Decimal,
    /// Initial wallet collateral at bot startup.
    pub starting_collateral: Decimal,
    /// Crypto identifier (e.g., "BTC", "ETH", "SOL") for threshold lookups
    pub crypto_filter: String,
    /// Timestamp when the bot started trading the current market.
    /// Used by strategies to enforce a minimum market maturation period before entry.
    pub market_started_at: DateTime<Utc>,
    /// Optional dedicated maker venue (window or daily market).
    /// When set, MakerStrategy uses this market for GTD order placement instead of
    /// the primary hourly market. This gives passive orders more time to fill and
    /// avoids the thin liquidity at hourly market open.
    pub maker_market: Option<MarketConfig>,
    /// Live orderbook snapshot for the maker venue.
    /// Paired with maker_market — always Some when maker_market is Some, None otherwise.
    pub maker_snapshot: Option<MarketSnapshot>,
    /// Live pUSD collateral balance, updated every 60s.
    /// Strategies should gate on this to avoid generating signals when the wallet cannot
    /// afford even the minimum trade + fee, preventing 400 rejections from the CLOB.
    pub available_collateral: Decimal,
    /// Runtime-tunable strategy parameters loaded from SQLite.
    /// Snapshot is taken once per tick from the watch channel so strategies always
    /// read the latest values without any locking overhead.
    /// Hot-patched by the Control Tower API via `DynamicConfig::apply_patch`.
    pub dynamic_config: Arc<DynamicConfig>,
    /// Session-scoped per-market arbitrage re-entry lockouts. When the arb viper
    /// commits a pair to a market it inserts both tokens here; subsequent ticks see
    /// the lock and refuse to open a second pair on the same market (hold-to-settle,
    /// no churn). `None` for venues/tests that don't supply it.
    pub arb_market_lockouts: Option<crate::state::ArbMarketLockouts>,
    /// Clock seam — wall-clock "now" for this tick.
    ///
    /// In production this is `Utc::now()` captured at snapshot-build time, so every
    /// viper gate reads a single consistent wall clock instead of calling
    /// `Utc::now()` itself (behaviour-identical, captured microseconds earlier). The
    /// backtest harness overrides it with the replayed HISTORICAL timestamp so
    /// warmup/staleness/expiry/hold-time gates evaluate against the historical clock
    /// at any replay speed. Vipers MUST read this instead of `chrono::Utc::now()`
    /// inside `evaluate_entry`/`evaluate_exit`.
    pub wall_now: DateTime<Utc>,
    /// Clock seam — monotonic "now" for this tick.
    ///
    /// In production this is `std::time::Instant::now()` captured at snapshot-build
    /// time; the backtest harness maps it onto a synthetic monotonic timeline
    /// (`base + (t - t0)`) so per-viper cooldown timers (stored as `Instant`s)
    /// measure historical elapsed time under replay. Vipers MUST read this instead
    /// of `std::time::Instant::now()` inside their evaluate paths, and MUST stamp
    /// cooldown state with it so the later comparison is consistent.
    pub mono_now: Instant,
    /// Replay-isolation flag — `false` in production (default), `true` only under the
    /// backtest harness.
    ///
    /// When `true`, vipers MUST NOT consult the LIVE bot's persistent SQLite state
    /// (e.g. TrendReversal's cross-restart cascade guard reads the live trades table).
    /// An in-process replay run shares the process's DB pool registry, so an unguarded
    /// lookup would leak the live stop-loss history into the simulation — read-only, but
    /// a fidelity leak. Production behaviour with `false` is byte-identical to before this
    /// flag existed.
    pub is_replay: bool,
}

/// Trait that all strategies must implement.
/// Enables uniform handling and future per-strategy threading.
#[async_trait::async_trait]
pub trait Strategy: Send + Sync {
    /// Evaluate if strategy should execute an entry.
    async fn evaluate_entry(&self, ctx: &StrategyContext) -> Result<StrategySignal>;

    /// Evaluate if strategy should execute an exit.
    async fn evaluate_exit(&self, ctx: &StrategyContext) -> Result<StrategySignal>;

    /// Get current status of the strategy (for monitoring/lifecycle).
    fn status(&self) -> StrategyStatus;

    /// Strategy name for logging and identification.
    fn name(&self) -> String;

    /// Venue label shown in the startup attachment log (e.g. "Hourly", "Window/Daily").
    /// Default: "Hourly"
    fn venue(&self) -> &'static str { "Hourly" }

    /// Maximum USDC exposure budget for this strategy.
    /// Default: 0 (override in each strategy impl)
    fn max_exposure(&self) -> Decimal { Decimal::ZERO }

    /// Risk model label shown in the startup attachment log.
    /// Default: "Unknown"
    fn risk_model(&self) -> &'static str { "Unknown" }
}
