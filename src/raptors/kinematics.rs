//! Price kinematics — velocity / acceleration / drift math shared by every
//! price-bearing raptor.
//!
//! This is the exact momentum/drift computation that used to live inline in
//! `price.rs`, factored into a reusable struct so a second source (Hyperliquid
//! trades) produces byte-identical `velocity_tx` / `drift_tx` signals from the
//! same windows and history semantics.
//!
//! Semantics preserved verbatim from the original `run_price_raptor`:
//!   • velocity_5s  — Δprice over the primary window (`MOMENTUM_WINDOW_SECS`, 5s)
//!   • velocity_1s  — Δprice over the short window (`MOMENTUM_SHORT_WINDOW_SECS`, 1s);
//!                    falls back to velocity_5s when no sub-1s history exists yet
//!   • acceleration — Δ(velocity_5s) since the previous tick
//!   • drift_60m    — Δprice over the trailing 60m window, active once
//!                    ≥`DRIFT_60M_MIN_WINDOW_SECS` of history exists (graceful
//!                    degradation so the exhaustion gate isn't blind after restart)
//!   • drift_10m    — Δprice over the trailing 10m window, active once ≥60s of
//!                    history exists (fills the 5s–60m gap for GBoost feature [18])
//!
//! Reconnect semantics (`reset_velocity`) also mirror `price.rs`: on a WS
//! reconnect the previous-velocity accumulator is zeroed and the 10-minute
//! history is cleared, while the 5s and 60m histories are left to self-trim by
//! elapsed time.

use std::collections::VecDeque;

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tokio::time::Instant;

use crate::config;

/// One tick's worth of derived momentum/drift signals.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Kinematics {
    pub velocity_5s: Decimal,
    pub velocity_1s: Decimal,
    pub acceleration: Decimal,
    pub drift_60m: Decimal,
    pub drift_10m: Decimal,
}

/// Rolling price-history accumulator that derives momentum + drift signals.
pub struct PriceKinematics {
    /// Primary momentum window (`MOMENTUM_WINDOW_SECS`, 5s).
    price_history: VecDeque<(Instant, Decimal)>,
    /// 60-minute drift history.
    price_history_60m: VecDeque<(Instant, Decimal)>,
    /// 10-minute drift history.
    price_history_10m: VecDeque<(Instant, Decimal)>,
    /// Previous 5s velocity, for the acceleration derivative.
    prev_velocity: Decimal,
}

impl PriceKinematics {
    pub fn new() -> Self {
        Self {
            price_history: VecDeque::new(),
            price_history_60m: VecDeque::new(),
            price_history_10m: VecDeque::new(),
            prev_velocity: dec!(0),
        }
    }

    /// Feed one `(now, price)` tick and return the latest derived signals.
    ///
    /// The math here is a 1:1 port of the original inline block in `price.rs`;
    /// changing it changes every downstream viper threshold, so it must not
    /// drift.
    pub fn on_price(&mut self, now: Instant, price: Decimal) -> Kinematics {
        self.price_history.push_back((now, price));

        // Trim entries older than the primary window (5s).
        while let Some((t, _)) = self.price_history.front() {
            if now.duration_since(*t).as_secs() >= config::MOMENTUM_WINDOW_SECS {
                self.price_history.pop_front();
            } else {
                break;
            }
        }

        // Primary velocity (5s window).
        let velocity_5s = if let Some((_, start_price)) = self.price_history.front() {
            price - start_price
        } else {
            dec!(0)
        };

        // Short velocity (1s window).
        let velocity_1s = {
            let cutoff = config::MOMENTUM_SHORT_WINDOW_SECS;
            let start_1s = self
                .price_history
                .iter()
                .find(|(t, _)| now.duration_since(*t).as_secs() < cutoff);
            match start_1s {
                Some((_, p)) => price - p,
                None => velocity_5s,
            }
        };

        // Acceleration: rate of change of velocity.
        let acceleration = velocity_5s - self.prev_velocity;
        self.prev_velocity = velocity_5s;

        // 60-minute drift.
        self.price_history_60m.push_back((now, price));
        while let Some((t, _)) = self.price_history_60m.front() {
            if now.duration_since(*t).as_secs() > 3600 {
                self.price_history_60m.pop_front();
            } else {
                break;
            }
        }
        // Graceful degradation (mirrors drift_10m below): once at least
        // DRIFT_60M_MIN_WINDOW_SECS of history exists, report the drift over
        // whatever window IS available rather than staying 0 until a full
        // hour accrues.  The prior all-or-nothing `>= 3600s` check left the
        // Convergence 60m-exhaustion gate blind for a full hour after every
        // restart.  A shorter window yields a smaller drift → conservative.
        let drift_60m = if let Some((oldest_t, oldest_p)) = self.price_history_60m.front() {
            let window_secs = now.duration_since(*oldest_t).as_secs();
            if window_secs >= config::DRIFT_60M_MIN_WINDOW_SECS {
                price - oldest_p
            } else {
                dec!(0)
            }
        } else {
            dec!(0)
        };

        // 10-minute drift — fills the 5s–60m gap for GBoost feature [18].
        // Active once at least 60s of history exists, per the original fix so the
        // gate is live from the second minute rather than silent for 10 minutes.
        self.price_history_10m.push_back((now, price));
        while let Some((t, _)) = self.price_history_10m.front() {
            if now.duration_since(*t).as_secs() > 600 {
                self.price_history_10m.pop_front();
            } else {
                break;
            }
        }
        let drift_10m = if let Some((oldest_t, oldest_p)) = self.price_history_10m.front() {
            let window_secs = now.duration_since(*oldest_t).as_secs();
            if window_secs >= 60 {
                price - oldest_p
            } else {
                dec!(0)
            }
        } else {
            dec!(0)
        };

        Kinematics {
            velocity_5s,
            velocity_1s,
            acceleration,
            drift_60m,
            drift_10m,
        }
    }

    /// Reset the velocity accumulator on reconnect — exactly what `price.rs`
    /// does in its disconnect branch: zero `prev_velocity` and clear the 10m
    /// history (the 5s / 60m histories self-trim by elapsed time).
    pub fn reset_velocity(&mut self) {
        self.prev_velocity = dec!(0);
        self.price_history_10m.clear();
    }

    /// The rolling 60-minute price series as `f64`, oldest first — feeds the
    /// shared realized-volatility telemetry (`helpers::volatility`).
    pub fn prices_60m_f64(&self) -> Vec<f64> {
        use rust_decimal::prelude::ToPrimitive as _;
        self.price_history_60m
            .iter()
            .map(|(_, p)| p.to_f64().unwrap_or(0.0))
            .collect()
    }
}

impl Default for PriceKinematics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Duration;

    #[test]
    fn velocity_over_primary_window() {
        let mut k = PriceKinematics::new();
        let t0 = Instant::now();

        // First tick: no prior history, all zeros.
        let a = k.on_price(t0, dec!(100));
        assert_eq!(a.velocity_5s, dec!(0));
        assert_eq!(a.velocity_1s, dec!(0));
        assert_eq!(a.acceleration, dec!(0));

        // +2s, price +10: front is still t0 within the 5s window.
        let b = k.on_price(t0 + Duration::from_secs(2), dec!(110));
        assert_eq!(b.velocity_5s, dec!(10)); // 110 - 100
        assert_eq!(b.velocity_1s, dec!(0)); // only the fresh tick is <1s old
        assert_eq!(b.acceleration, dec!(10)); // 10 - 0

        // +6s: the t0 entry (age 6 ≥ 5) is trimmed; front becomes the +2s@110.
        let c = k.on_price(t0 + Duration::from_secs(6), dec!(120));
        assert_eq!(c.velocity_5s, dec!(10)); // 120 - 110
        assert_eq!(c.acceleration, dec!(0)); // 10 - 10
    }

    #[test]
    fn drift_10m_requires_60s_of_history() {
        let mut k = PriceKinematics::new();
        let t0 = Instant::now();

        k.on_price(t0, dec!(100));
        // Under 60s of history → drift_10m stays 0.
        let short = k.on_price(t0 + Duration::from_secs(30), dec!(130));
        assert_eq!(short.drift_10m, dec!(0));

        // Fresh accumulator, ≥60s span → drift_10m becomes active.
        let mut k2 = PriceKinematics::new();
        k2.on_price(t0, dec!(100));
        let long = k2.on_price(t0 + Duration::from_secs(65), dec!(130));
        assert_eq!(long.drift_10m, dec!(30)); // 130 - 100
    }

    #[test]
    fn drift_60m_degrades_gracefully_from_min_window() {
        let mut k = PriceKinematics::new();
        let t0 = Instant::now();

        k.on_price(t0, dec!(100));
        // Under DRIFT_60M_MIN_WINDOW_SECS of history → 60m drift is still 0.
        let early = k.on_price(
            t0 + Duration::from_secs(config::DRIFT_60M_MIN_WINDOW_SECS - 1),
            dec!(180),
        );
        assert_eq!(early.drift_60m, dec!(0));

        // Once the minimum window accrues, the drift over the AVAILABLE window
        // activates (graceful degradation — no full hour required).
        let mut k2 = PriceKinematics::new();
        k2.on_price(t0, dec!(100));
        let mid = k2.on_price(
            t0 + Duration::from_secs(config::DRIFT_60M_MIN_WINDOW_SECS),
            dec!(180),
        );
        assert_eq!(mid.drift_60m, dec!(80)); // 180 - 100

        // A full hour of span still measures against the oldest point.
        let mut k3 = PriceKinematics::new();
        k3.on_price(t0, dec!(100));
        let hour = k3.on_price(t0 + Duration::from_secs(3600), dec!(250));
        assert_eq!(hour.drift_60m, dec!(150)); // 250 - 100
    }

    #[test]
    fn reset_velocity_zeros_accumulator_and_clears_10m() {
        let mut k = PriceKinematics::new();
        let t0 = Instant::now();

        k.on_price(t0, dec!(100));
        k.on_price(t0 + Duration::from_secs(65), dec!(130));

        k.reset_velocity();

        // After reset the acceleration derivative restarts from 0, and the 10m
        // history was cleared so drift_10m needs to rebuild its 60s window.
        let after = k.on_price(t0 + Duration::from_secs(66), dec!(140));
        // velocity_5s here: 5s window still holds the +65s@130 tick (age 1s),
        // so 140 - 130 = 10, and acceleration = 10 - 0 (reset) = 10.
        assert_eq!(after.acceleration, after.velocity_5s);
        assert_eq!(after.drift_10m, dec!(0));
    }
}
