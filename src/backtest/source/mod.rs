//! Historical-market-data providers behind a DRADIS-owned trait.
//!
//! Mirrors `helpers::llm_client::LlmChat` (runtime-selected backend, boxed once
//! per run, one factory), NOT `venues::core::Execution` (compile-time static
//! dispatch) — the data source is a per-CLI-invocation choice, not a build flavor.

use anyhow::Result;
use async_trait::async_trait;

use super::fetch::{Candle, FundingPoint}; // existing DTOs, unchanged

pub mod binance;
pub mod hyperliquid;

pub use binance::BinanceSource;
pub use hyperliquid::HyperliquidSource;

/// DRADIS-owned contract for one historical-market-data provider (candles +
/// funding history).
///
/// Implementors own ONLY the wire protocol: endpoint, request shape, their own
/// pagination loop (caps/cursors/politeness delay), and field parsing into
/// `Decimal`. Both current providers hit PUBLIC, unauthenticated endpoints —
/// no API keys anywhere (Hyperliquid's info API has no auth field; Binance
/// market data is IP-rate-limited and ignores keys). Caching (SQLite),
/// coverage checks, and the "never persist a still-forming candle" rule live
/// in `BacktestCache`, not here — that split is the whole point.
///
/// Future data kinds (e.g. open-interest history) are added as new trait
/// methods with `Ok(Vec::new())`-style defaults, exactly like
/// `Execution::open_orders`/`subscribe_fills` already do in `venues/core.rs`,
/// so existing providers keep compiling.
#[async_trait]
pub trait HistoricalSource: Send + Sync {
    /// Canonical lowercase id — the cache-key namespace (`source` column) and
    /// the tag in logs/`report.json`. MUST be stable: renaming it orphans
    /// previously cached rows under the old id (a harmless cache-miss/refetch,
    /// not corruption — but for Hyperliquid, whose API retains only ~5000
    /// candles, orphaned history is unrefetchable).
    fn id(&self) -> &'static str;

    /// Map a DRADIS coin symbol (e.g. "BTC" — `BacktestConfig::coin`) onto
    /// this provider's native wire symbol (Hyperliquid: "BTC"; Binance FAPI:
    /// "BTCUSDT"). Called ONLY inside `fetch_candles`/`fetch_funding` — the
    /// cache and harness always key on the DRADIS coin, never the wire symbol,
    /// so cache rows read the same ("BTC") regardless of `--source`.
    fn resolve_symbol(&self, coin: &str) -> String;

    /// This provider's native funding-rate cadence in hours (Hyperliquid = 1;
    /// Binance FAPI majors = 8). Used by `synth::normalize_funding` to rescale
    /// onto the canonical per-8h unit, and by `BacktestCache::load_funding`
    /// for its coverage-slack math.
    fn funding_period_hours(&self) -> u32;

    /// Fetch every candle in `[start_ms, end_ms)` for `coin`/`interval`,
    /// paginating internally per this provider's own limits. Ascending by
    /// open-time `ts_ms`. MAY include the still-forming final bar — the cache
    /// strips it on insert, not the provider.
    async fn fetch_candles(
        &self,
        coin: &str,
        interval: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<Candle>>;

    /// Fetch every funding observation in `[start_ms, end_ms)` for `coin`,
    /// paginating internally, ascending. Rates are returned in the PROVIDER'S
    /// NATIVE cadence — normalization happens in `synth::normalize_funding`.
    async fn fetch_funding(&self, coin: &str, start_ms: i64, end_ms: i64)
        -> Result<Vec<FundingPoint>>;
}

/// Which provider a backtest reads from. Runtime-selected per run via
/// `--source` (CLI) or the HTTP API's `source` field — NOT a cargo feature:
/// both backends are cheap HTTP+JSON with no SDK, so there is nothing to
/// compile out. Derives `clap::ValueEnum` so the CLI and the HTTP API share
/// ONE parser (`<SourceKind as clap::ValueEnum>::from_str(s, true)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SourceKind {
    Hyperliquid,
    Binance,
}

impl SourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceKind::Hyperliquid => "hyperliquid",
            SourceKind::Binance => "binance",
        }
    }
}

/// The one factory — mirrors `llm_client::build_client`. Adding a provider =
/// one enum variant + one file + one match arm.
pub fn build_source(kind: SourceKind) -> Box<dyn HistoricalSource> {
    match kind {
        SourceKind::Hyperliquid => Box::new(HyperliquidSource::new()),
        SourceKind::Binance => Box::new(BinanceSource::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum;

    #[test]
    fn source_kind_parses_case_insensitively_via_value_enum() {
        assert_eq!(
            <SourceKind as ValueEnum>::from_str("hyperliquid", true).unwrap(),
            SourceKind::Hyperliquid
        );
        assert_eq!(
            <SourceKind as ValueEnum>::from_str("HyperLiquid", true).unwrap(),
            SourceKind::Hyperliquid
        );
        assert_eq!(
            <SourceKind as ValueEnum>::from_str("BINANCE", true).unwrap(),
            SourceKind::Binance
        );
        assert_eq!(
            <SourceKind as ValueEnum>::from_str("binance", true).unwrap(),
            SourceKind::Binance
        );
    }

    #[test]
    fn source_kind_rejects_unknown_value() {
        assert!(<SourceKind as ValueEnum>::from_str("kraken", true).is_err());
    }

    #[test]
    fn source_kind_as_str_round_trips_through_value_enum() {
        for kind in SourceKind::value_variants() {
            let parsed = <SourceKind as ValueEnum>::from_str(kind.as_str(), true).unwrap();
            assert_eq!(parsed, *kind);
        }
    }

}
