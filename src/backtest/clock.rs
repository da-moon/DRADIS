//! W3 — Replay clock: maps historical candle timestamps onto a synthetic monotonic
//! timeline so the W1 clock seam evaluates every viper gate against HISTORICAL time.
//!
//! For a candle at historical time `t` (unix millis):
//!
//! * `wall(t)` = the historical `DateTime<Utc>` (fed to `ctx.wall_now`,
//!   `snapshot.timestamp`, and used for `secs_to_expiry`).
//! * `mono(t)` = `base_instant + (t - t0)` — a synthetic monotonic `Instant`
//!   (fed to `ctx.mono_now` as `std` and to `PriceKinematics::on_price` as
//!   `tokio::time::Instant`; both only ever use relative offsets).
//!
//! Because `PriceKinematics::on_price` and every cooldown timer read ONLY the passed
//! `now`, feeding these synthetic instants yields byte-identical signal math to a
//! live run at any replay speed (the kinematics unit tests prove exactly this).

use chrono::{DateTime, TimeZone, Utc};
use std::time::Duration;

pub struct ReplayClock {
    t0_ms: i64,
    base_std: std::time::Instant,
    base_tokio: tokio::time::Instant,
}

impl ReplayClock {
    /// Anchor the synthetic timeline at historical time `t0_ms` (the first candle).
    pub fn new(t0_ms: i64) -> Self {
        Self {
            t0_ms,
            base_std: std::time::Instant::now(),
            base_tokio: tokio::time::Instant::now(),
        }
    }

    /// Wall-clock `DateTime<Utc>` for a historical unix-millis timestamp.
    pub fn wall(&self, ts_ms: i64) -> DateTime<Utc> {
        Utc.timestamp_millis_opt(ts_ms)
            .single()
            .unwrap_or_else(|| Utc.timestamp_opt(0, 0).single().unwrap())
    }

    /// Monotonic offset from the anchor (saturating at zero for pre-anchor stamps).
    fn offset(&self, ts_ms: i64) -> Duration {
        Duration::from_millis((ts_ms - self.t0_ms).max(0) as u64)
    }

    /// Synthetic `std::time::Instant` for `ctx.mono_now` and viper cooldown timers.
    pub fn mono_std(&self, ts_ms: i64) -> std::time::Instant {
        self.base_std + self.offset(ts_ms)
    }

    /// Synthetic `tokio::time::Instant` for `PriceKinematics::on_price`.
    pub fn mono_tokio(&self, ts_ms: i64) -> tokio::time::Instant {
        self.base_tokio + self.offset(ts_ms)
    }

    /// Seconds from `ts_ms` until `close` (negative once past close).
    pub fn secs_to_expiry(&self, ts_ms: i64, close: DateTime<Utc>) -> i64 {
        (close - self.wall(ts_ms)).num_seconds()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_roundtrips_millis() {
        let c = ReplayClock::new(1_700_000_000_000);
        assert_eq!(c.wall(1_700_000_000_000).timestamp_millis(), 1_700_000_000_000);
    }

    #[test]
    fn mono_offset_is_relative_and_monotonic() {
        let c = ReplayClock::new(1_000_000);
        let a = c.mono_std(1_000_000);
        let b = c.mono_std(1_060_000); // +60s
        assert_eq!(b.duration_since(a), Duration::from_secs(60));
        // Pre-anchor saturates to the base (no panic, no underflow).
        assert_eq!(c.mono_std(0).duration_since(a), Duration::ZERO);
    }

    #[test]
    fn secs_to_expiry_counts_down() {
        let c = ReplayClock::new(0);
        let close = c.wall(3_600_000); // 1h after anchor
        assert_eq!(c.secs_to_expiry(0, close), 3600);
        assert_eq!(c.secs_to_expiry(3_000_000, close), 600);
    }
}
