//! Market-data source selection + per-source symbol resolution.
//!
//! DRADIS reads live crypto telemetry (spot price, velocity, funding, open
//! interest, taker flow) from a market-data *source*. Historically that was
//! always Binance; this module makes the source runtime-selectable so an
//! operator can point the raptor layer at Hyperliquid instead (e.g. in regions
//! where Binance is unreachable) without touching any strategy code — the
//! raptors feed the SAME `watch` channels regardless of source.
//!
//! Selection precedence (highest first):
//!   1. `MARKET_DATA_SOURCE` env var
//!   2. `config::MARKET_DATA_SOURCE` compile-time constant
//!   3. `Binance` (hard default — existing deployments are unchanged)
//!
//! Degrade-don't-panic (repo convention): an unknown value, or `hyperliquid`
//! selected in a build without the `hyperliquid` cargo feature, logs an
//! `error!` listing the valid values and falls back to Binance.
//!
//! This module also centralises the per-venue symbol resolution that was
//! previously duplicated across `price.rs`, `funding.rs`, `derivatives.rs`, and
//! `helpers/time.rs` (six near-identical `match` blocks). The Binance
//! resolvers preserve the exact `_ => btc` fallback of those sites so behaviour
//! is byte-identical when the source is unset.

use tracing::error;

use crate::config;

/// Which external market-data source the raptor layer reads from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarketDataSource {
    /// Binance Spot WS (price) + Binance FAPI REST (funding, OI/CVD). Default.
    Binance,
    /// Hyperliquid Info API — one WS raptor per asset (trades + activeAssetCtx).
    Hyperliquid,
}

impl MarketDataSource {
    /// Resolve the active source from `MARKET_DATA_SOURCE` env → config → Binance.
    pub fn resolve() -> Self {
        Self::resolve_with(
            std::env::var("MARKET_DATA_SOURCE").ok().as_deref(),
            config::MARKET_DATA_SOURCE,
        )
    }

    /// Precedence-aware resolution split out for unit testing (env > config).
    fn resolve_with(env_val: Option<&str>, config_val: &str) -> Self {
        let raw = env_val
            .map(str::to_string)
            .unwrap_or_else(|| config_val.to_string());
        Self::parse(&raw)
    }

    /// Parse a raw source string, applying the degrade-don't-panic fallbacks.
    fn parse(raw: &str) -> Self {
        match raw.trim().to_lowercase().as_str() {
            "binance" => MarketDataSource::Binance,
            "hyperliquid" => {
                #[cfg(feature = "hyperliquid")]
                {
                    MarketDataSource::Hyperliquid
                }
                #[cfg(not(feature = "hyperliquid"))]
                {
                    error!(
                        "⚠️ MARKET_DATA_SOURCE=hyperliquid but this build was compiled \
                         without the `hyperliquid` cargo feature — falling back to Binance. \
                         Rebuild with default features (or `--features hyperliquid`) to enable it."
                    );
                    MarketDataSource::Binance
                }
            }
            other => {
                error!(
                    "⚠️ Unknown MARKET_DATA_SOURCE '{}' (valid: binance | hyperliquid) — \
                     falling back to Binance.",
                    other
                );
                MarketDataSource::Binance
            }
        }
    }

    /// Canonical lowercase identifier for logs, telemetry, and the status API.
    pub fn as_str(&self) -> &'static str {
        match self {
            MarketDataSource::Binance => "binance",
            MarketDataSource::Hyperliquid => "hyperliquid",
        }
    }
}

// ─── Centralised venue-symbol resolution ────────────────────────────────────
//
// These replace the six duplicated `match crypto_filter { … }` blocks that used
// to live in price.rs / funding.rs / derivatives.rs / helpers/time.rs. The
// `_ => btc` fallback is preserved verbatim from those sites so an unrecognised
// asset slug keeps resolving to BTC exactly as before.

/// Binance Spot WS pair (lowercase), e.g. `"btc"` → `"btcusdt"`.
pub fn binance_ws_pair(asset: &str) -> String {
    match asset {
        "eth" => "ethusdt",
        "sol" => "solusdt",
        _ => "btcusdt",
    }
    .to_string()
}

/// Binance REST/FAPI symbol (uppercase), e.g. `"btc"` → `"BTCUSDT"`.
pub fn binance_symbol(asset: &str) -> String {
    match asset {
        "eth" => "ETHUSDT",
        "sol" => "SOLUSDT",
        _ => "BTCUSDT",
    }
    .to_string()
}

/// Hyperliquid perp coin (uppercase, plain symbol), e.g. `"btc"` → `"BTC"`.
pub fn hyperliquid_coin(asset: &str) -> String {
    match asset {
        "eth" => "ETH",
        "sol" => "SOL",
        _ => "BTC",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_precedence_env_over_config() {
        // Explicit env value wins over the config default.
        assert_eq!(
            MarketDataSource::resolve_with(Some("binance"), "hyperliquid"),
            MarketDataSource::Binance
        );
        // Falls back to config when env is absent.
        assert_eq!(
            MarketDataSource::resolve_with(None, "binance"),
            MarketDataSource::Binance
        );
    }

    #[test]
    fn resolve_unknown_value_falls_back_to_binance() {
        assert_eq!(
            MarketDataSource::resolve_with(Some("coinbase"), "binance"),
            MarketDataSource::Binance
        );
        assert_eq!(
            MarketDataSource::resolve_with(None, "garbage"),
            MarketDataSource::Binance
        );
    }

    #[test]
    fn parse_is_case_and_whitespace_insensitive() {
        assert_eq!(MarketDataSource::parse("  BINANCE  "), MarketDataSource::Binance);
    }

    #[cfg(feature = "hyperliquid")]
    #[test]
    fn resolve_hyperliquid_when_feature_enabled() {
        assert_eq!(
            MarketDataSource::resolve_with(Some("hyperliquid"), "binance"),
            MarketDataSource::Hyperliquid
        );
        assert_eq!(
            MarketDataSource::parse("HyperLiquid"),
            MarketDataSource::Hyperliquid
        );
    }

    #[cfg(not(feature = "hyperliquid"))]
    #[test]
    fn resolve_hyperliquid_falls_back_without_feature() {
        assert_eq!(
            MarketDataSource::resolve_with(Some("hyperliquid"), "binance"),
            MarketDataSource::Binance
        );
    }

    #[test]
    fn as_str_roundtrip() {
        assert_eq!(MarketDataSource::Binance.as_str(), "binance");
        assert_eq!(MarketDataSource::Hyperliquid.as_str(), "hyperliquid");
    }

    #[test]
    fn binance_symbol_resolution_matches_legacy_fallback() {
        assert_eq!(binance_ws_pair("btc"), "btcusdt");
        assert_eq!(binance_ws_pair("eth"), "ethusdt");
        assert_eq!(binance_ws_pair("sol"), "solusdt");
        // Unknown slug preserves the historical `_ => btc` fallback.
        assert_eq!(binance_ws_pair("doge"), "btcusdt");

        assert_eq!(binance_symbol("btc"), "BTCUSDT");
        assert_eq!(binance_symbol("eth"), "ETHUSDT");
        assert_eq!(binance_symbol("sol"), "SOLUSDT");
        assert_eq!(binance_symbol("doge"), "BTCUSDT");
    }

    #[test]
    fn hyperliquid_coin_resolution() {
        assert_eq!(hyperliquid_coin("btc"), "BTC");
        assert_eq!(hyperliquid_coin("eth"), "ETH");
        assert_eq!(hyperliquid_coin("sol"), "SOL");
        // Unknown slug preserves the `_ => btc` fallback.
        assert_eq!(hyperliquid_coin("doge"), "BTC");
    }
}
