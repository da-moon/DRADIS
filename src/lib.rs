pub mod config;
pub mod venues;
pub mod vipers;
pub mod helpers;
pub mod state;
pub mod orchestrator;
pub mod tasks;
pub mod raptors;
pub mod squadron;
pub mod cag;
pub mod api;

/// Historical-replay backtesting subsystem. Feature-gated (`backtest`) so default
/// builds never pull rs-backtester's plotting/font stack. Replays real Hyperliquid
/// candles + funding through the REAL viper `Strategy` objects behind the W1 clock
/// seam, keeps an authoritative Decimal ledger with binary settlement, and reports
/// via rs-backtester metrics + native CSV/JSON.
#[cfg(feature = "backtest")]
pub mod backtest;
