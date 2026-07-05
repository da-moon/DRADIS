/// Raptor recon layer — lightweight signal scouts that fly ahead of the Vipers
/// and report external intelligence back to the CIC.
///
/// Each Raptor polls a specific external data source on its own schedule and
/// publishes a normalised signal the Viper strategies consume via `watch` channels.
/// Raptors are intentionally dumb: they fetch, normalise, and broadcast — no
/// trading logic, no position awareness, no side effects.
///
/// Current Raptors
/// ───────────────
/// │ Raptor          │ Source               │ Signal                                   │
/// │─────────────────│──────────────────────│──────────────────────────────────────────│
/// │ Price Raptor    │ Binance Spot WS      │ spot price, 5s/1s velocity, accel, drift │
/// │ Funding Raptor  │ Binance FAPI REST    │ perpetual funding rate (smart-money)     │
/// │ Tide Raptor     │ Binance oracle + IEX │ ETF "Institutional Pulse" + coherence    │
///
/// Future Raptors (not yet implemented)
/// ─────────────────────────────────────
/// │ Sports Raptor   │ Line-movement APIs   │ betting-line drift, public money %       │
/// │ Politics Raptor │ Polling aggregators  │ approval drift, event probability shifts │
///
/// When multiple Raptors are active the GBoost and Basis strategies fuse their
/// signals as features — no single Raptor has veto power alone.
pub mod price;
pub mod funding;
pub mod derivatives;
pub mod tide;

/// Market-data source selection + centralized per-venue symbol resolution.
pub mod source;
/// Shared velocity / acceleration / drift math (used by price + hyperliquid).
pub mod kinematics;

/// Hyperliquid raptor — one WS task per asset feeding every raptor channel.
/// Gated behind the additive `hyperliquid` cargo feature (pulls in the SDK).
#[cfg(feature = "hyperliquid")]
pub mod hyperliquid;
