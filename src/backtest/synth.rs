//! W4 — Snapshot synthesizer: historical candles → `MarketSnapshot`.
//!
//! * **Kinematics** — 1m closes are fed through a shared `PriceKinematics` at 60s
//!   synthetic steps, producing `oracle_drift_10m`/`drift_60m` faithfully (10/60
//!   one-minute samples). `velocity_5s`/`velocity_1s` fall to ~0 because the 5s/1s
//!   windows never contain a second sample at 60s spacing — velocity-gated logic is
//!   therefore **Tier B** (approximate), as documented.
//! * **Funding** — the provider-native funding rate is rescaled onto Binance's per-8h
//!   scale via [`normalize_funding`] (generalized from the HL-only ×8 rescale that
//!   [`normalize_funding_8h`] still performs as a back-compat alias; mirrors
//!   `raptors::hyperliquid`), then fed to `snapshot.funding_rate`. Funding is a signal
//!   input only — binary shares pay no carry.
//! * **Derivatives / tide** — `oi_delta_pct = cvd_ratio = institutional_pulse =
//!   tide_coherence = 0`: no historical source. Convergence no-ops by design (Tier C).
//! * **Polymarket book** — the 8 book fields come from [`book_model`], a pure,
//!   unit-tested binary-option pricing of YES = P(finish above strike) with a
//!   configurable half-spread and constant depth. Sweepable in isolation (Tier C).

use std::collections::VecDeque;

use chrono::{DateTime, Utc};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::raptors::kinematics::PriceKinematics;
use crate::state::MarketSnapshot;

use super::clock::ReplayClock;
use super::fetch::Candle;

/// Normalize a provider-native funding rate onto Binance's canonical per-8h
/// scale. `period_hours` is the provider's native cadence
/// (`HistoricalSource::funding_period_hours`: Hyperliquid = 1, Binance = 8).
pub fn normalize_funding(rate: Decimal, period_hours: u32) -> Decimal {
    if period_hours == 8 {
        return rate; // already at target scale — avoid needless 8/8 arithmetic
    }
    rate * Decimal::from(8u32) / Decimal::from(period_hours.max(1))
}

/// Back-compat alias (HL hourly ×8), exactly as
/// `raptors::hyperliquid::normalize_funding_8h` does. Re-implemented here (that fn is
/// private AND behind the `hyperliquid` cargo feature, which `backtest` does not
/// require) — keeps every existing call site and the drift-check test vs
/// `raptors::hyperliquid`'s private copy compiling as-is.
pub fn normalize_funding_8h(hourly: Decimal) -> Decimal {
    normalize_funding(hourly, 1)
}

/// The 8 modeled Polymarket book fields for one tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BookQuote {
    pub yes_bid: Decimal,
    pub yes_ask: Decimal,
    pub yes_bid_depth: Decimal,
    pub yes_ask_depth: Decimal,
    pub no_bid: Decimal,
    pub no_ask: Decimal,
    pub no_bid_depth: Decimal,
    pub no_ask_depth: Decimal,
}

/// Standard normal CDF Φ(x) via a high-accuracy erf approximation
/// (Abramowitz & Stegun 7.1.26; |error| < 1.5e-7).
fn phi(x: f64) -> f64 {
    // erf(z) with the A&S rational approximation.
    fn erf(z: f64) -> f64 {
        let sign = if z < 0.0 { -1.0 } else { 1.0 };
        let z = z.abs();
        let t = 1.0 / (1.0 + 0.3275911 * z);
        let y = 1.0
            - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t
                + 0.254829592)
                * t
                * (-z * z).exp();
        sign * y
    }
    0.5 * (1.0 + erf(x / std::f64::consts::SQRT_2))
}

fn clamp_prob(v: Decimal) -> Decimal {
    v.clamp(dec!(0.01), dec!(0.99))
}

/// Pure binary-option book model. `sigma_per_min` is the per-minute log-return
/// volatility; `tau_min` is minutes to expiry. YES mid = P(oracle finishes above
/// strike) under a driftless log-normal, half-spread applied symmetrically, constant
/// depth per side, all clamped to `[0.01, 0.99]`. With no strike or no vol/time it
/// falls back to a 0.50 coin-flip mid.
pub fn book_model(
    oracle: Decimal,
    strike: Option<Decimal>,
    sigma_per_min: f64,
    tau_min: f64,
    half_spread: Decimal,
    depth: Decimal,
) -> BookQuote {
    let yes_mid = match strike {
        Some(k) if k > dec!(0) && sigma_per_min > 0.0 && tau_min > 0.0 => {
            let o = oracle.to_f64().unwrap_or(0.0);
            let kf = k.to_f64().unwrap_or(0.0);
            if o > 0.0 && kf > 0.0 {
                let vol = sigma_per_min * tau_min.sqrt();
                let d = (o / kf).ln() / vol;
                Decimal::from_f64_retain(phi(d)).unwrap_or(dec!(0.5))
            } else {
                dec!(0.5)
            }
        }
        _ => dec!(0.5),
    };
    let yes_mid = clamp_prob(yes_mid);
    let no_mid = dec!(1) - yes_mid;

    BookQuote {
        yes_bid: clamp_prob(yes_mid - half_spread),
        yes_ask: clamp_prob(yes_mid + half_spread),
        yes_bid_depth: depth,
        yes_ask_depth: depth,
        no_bid: clamp_prob(no_mid - half_spread),
        no_ask: clamp_prob(no_mid + half_spread),
        no_bid_depth: depth,
        no_ask_depth: depth,
    }
}

/// Builds one `MarketSnapshot` per replay tick from a shared kinematics accumulator,
/// a trailing-close volatility window, and the configured book-model parameters.
pub struct SnapshotSynthesizer {
    kin: PriceKinematics,
    closes: VecDeque<Decimal>,
    sigma_window: usize,
    half_spread: Decimal,
    depth: Decimal,
}

impl SnapshotSynthesizer {
    pub fn new(half_spread: Decimal, depth: Decimal, sigma_window: usize) -> Self {
        Self {
            kin: PriceKinematics::new(),
            closes: VecDeque::new(),
            sigma_window: sigma_window.max(2),
            half_spread,
            depth,
        }
    }

    /// Estimate per-minute log-return volatility from the trailing close window.
    fn sigma_per_min(&self) -> f64 {
        if self.closes.len() < 2 {
            return 0.0;
        }
        let mut rets: Vec<f64> = Vec::with_capacity(self.closes.len() - 1);
        let mut prev: Option<f64> = None;
        for c in &self.closes {
            let v = c.to_f64().unwrap_or(0.0);
            if let Some(p) = prev {
                if p > 0.0 && v > 0.0 {
                    rets.push((v / p).ln());
                }
            }
            prev = Some(v);
        }
        if rets.len() < 2 {
            return 0.0;
        }
        let mean = rets.iter().sum::<f64>() / rets.len() as f64;
        let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (rets.len() as f64 - 1.0);
        var.sqrt()
    }

    /// Produce the `MarketSnapshot` for the candle at `candle.ts_ms`, given the
    /// active market's `close_time`/`strike` and the (raw, provider-native cadence)
    /// funding rate.
    pub fn on_tick(
        &mut self,
        clock: &ReplayClock,
        candle: &Candle,
        market_close: DateTime<Utc>,
        strike: Option<Decimal>,
        funding_native: Decimal,
        funding_period_hours: u32,
    ) -> MarketSnapshot {
        // Kinematics at 60s synthetic steps (drift faithful; velocity ~0 → Tier B).
        let k = self
            .kin
            .on_price(clock.mono_tokio(candle.ts_ms), candle.close);

        self.closes.push_back(candle.close);
        while self.closes.len() > self.sigma_window {
            self.closes.pop_front();
        }

        let secs_to_expiry = clock.secs_to_expiry(candle.ts_ms, market_close);
        let tau_min = (secs_to_expiry.max(0) as f64) / 60.0;
        let book = book_model(
            candle.close,
            strike,
            self.sigma_per_min(),
            tau_min,
            self.half_spread,
            self.depth,
        );

        MarketSnapshot {
            yes_bid: book.yes_bid,
            yes_bid_depth: book.yes_bid_depth,
            yes_ask: book.yes_ask,
            yes_ask_depth: book.yes_ask_depth,
            no_bid: book.no_bid,
            no_bid_depth: book.no_bid_depth,
            no_ask: book.no_ask,
            no_ask_depth: book.no_ask_depth,
            oracle_price: candle.close,
            velocity: k.velocity_5s,
            velocity_1s: k.velocity_1s,
            acceleration: k.acceleration,
            funding_rate: normalize_funding(funding_native, funding_period_hours),
            // Tier C — no historical source; Convergence no-ops on these.
            institutional_pulse: dec!(0),
            tide_coherence: dec!(0),
            oi_delta_pct: dec!(0),
            cvd_ratio: dec!(0),
            // Tier C — Horizon raptor (TradFi/VIX) and oracle realized-vol have no
            // feed in the synthetic price-only backtest; gates treat these as absent.
            tradfi_velocity: dec!(0),
            macro_coherence: dec!(0),
            vix_proxy: dec!(0),
            vix_velocity: dec!(0),
            hist_vol: dec!(0),
            oracle_drift_60m: k.drift_60m,
            oracle_drift_10m: k.drift_10m,
            secs_to_expiry,
            timestamp: clock.wall(candle.ts_ms),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn funding_normalized_x8_matches_raptor() {
        assert_eq!(normalize_funding_8h(dec!(0.0000125)), dec!(0.0001));
        assert_eq!(normalize_funding_8h(dec!(-0.0000125)), dec!(-0.0001));
        assert_eq!(normalize_funding_8h(dec!(0)), dec!(0));
    }

    #[test]
    fn normalize_funding_identity_at_target_cadence() {
        // period_hours == 8 is already the canonical scale — pass-through.
        assert_eq!(normalize_funding(dec!(0.0001), 8), dec!(0.0001));
        assert_eq!(normalize_funding(dec!(-0.0001), 8), dec!(-0.0001));
        assert_eq!(normalize_funding(dec!(0), 8), dec!(0));
    }

    #[test]
    fn normalize_funding_scales_hourly_rate_by_8() {
        // period_hours == 1 (Hyperliquid) rescales onto the per-8h target.
        assert_eq!(normalize_funding(dec!(0.0000125), 1), dec!(0.0001));
        assert_eq!(normalize_funding(dec!(-0.0000125), 1), dec!(-0.0001));
        assert_eq!(normalize_funding(dec!(0), 1), dec!(0));
    }

    #[test]
    fn phi_is_calibrated() {
        assert!((phi(0.0) - 0.5).abs() < 1e-6);
        assert!(phi(5.0) > 0.999);
        assert!(phi(-5.0) < 0.001);
        // Symmetry.
        assert!((phi(1.0) + phi(-1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn book_model_above_strike_favours_yes() {
        let q = book_model(
            Decimal::from_str("101000").unwrap(),
            Some(Decimal::from_str("100000").unwrap()),
            0.001,   // ~0.1%/min vol
            30.0,    // 30 min to expiry
            dec!(0.02),
            dec!(500),
        );
        // Oracle above strike → YES mid > 0.5, so yes_ask well above no_ask.
        assert!(q.yes_ask > q.no_ask);
        assert!(q.yes_bid < q.yes_ask);
        // Clamped into [0.01, 0.99].
        assert!(q.yes_ask <= dec!(0.99) && q.yes_bid >= dec!(0.01));
        assert_eq!(q.yes_bid_depth, dec!(500));
    }

    #[test]
    fn book_model_no_strike_is_coinflip() {
        let q = book_model(dec!(50000), None, 0.001, 30.0, dec!(0.02), dec!(500));
        assert_eq!(q.yes_ask, dec!(0.52));
        assert_eq!(q.yes_bid, dec!(0.48));
        assert_eq!(q.no_ask, dec!(0.52));
        assert_eq!(q.no_bid, dec!(0.48));
    }

    #[test]
    fn book_model_clamps_extremes() {
        // Deep in the money with high vol/time still clamps at 0.99/0.01.
        let q = book_model(
            Decimal::from_str("200000").unwrap(),
            Some(Decimal::from_str("100000").unwrap()),
            0.0005,
            10.0,
            dec!(0.02),
            dec!(500),
        );
        assert!(q.yes_ask <= dec!(0.99));
        assert!(q.no_bid >= dec!(0.01));
    }
}
