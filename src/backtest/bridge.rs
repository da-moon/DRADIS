//! W6 — f64⇄Decimal boundary + StrategySignal→rs_backtester::Order mapping.
//!
//! rs-backtester is f64-throughout (volume is `u64`) and its `Order{BUY,SHORTSELL,
//! NULL}` models a LINEAR instrument. The mapping here is therefore a **directional
//! proxy on the underlying**, NOT the binary YES/NO payoff — the authoritative PnL is
//! the native Decimal ledger. The proxy exists only to borrow rs-backtester's free
//! Sharpe/drawdown/win-rate metrics on the candle series:
//!
//!   * net long YES exposure  → `Order::BUY`     (long the underlying)
//!   * net long NO  exposure  → `Order::SHORTSELL`(short the underlying)
//!   * flat                   → `Order::NULL`
//!
//! The harness computes one [`Stance`] per candle from the net position direction
//! after that tick; `to_orders` lowers the aligned stance vector to rs-backtester's
//! `choices` (length == candle count, as `broker::calculate` requires).

use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

use rs_backtester::orders::Order;

/// Per-candle net directional stance (a directional proxy, not the binary payoff).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stance {
    /// Net long YES exposure → long the underlying.
    Long,
    /// Net long NO exposure → short the underlying.
    Short,
    /// Flat.
    Flat,
}

impl Stance {
    pub fn to_order(self) -> Order {
        match self {
            Stance::Long => Order::BUY,
            Stance::Short => Order::SHORTSELL,
            Stance::Flat => Order::NULL,
        }
    }
}

/// Lower an aligned stance vector to rs-backtester `choices`.
pub fn to_orders(stances: &[Stance]) -> Vec<Order> {
    stances.iter().map(|s| s.to_order()).collect()
}

/// Decimal → f64 for the rs-backtester f64 boundary (single conversion point).
pub fn dec_to_f64(d: Decimal) -> f64 {
    d.to_f64().unwrap_or(0.0)
}

/// Decimal → u64 for rs-backtester's `volume: Vec<u64>` (truncating, non-negative).
pub fn dec_to_u64(d: Decimal) -> u64 {
    d.max(Decimal::ZERO).to_u64().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn stance_maps_to_orders() {
        let s = [Stance::Long, Stance::Short, Stance::Flat, Stance::Long];
        let o = to_orders(&s);
        assert_eq!(o, vec![Order::BUY, Order::SHORTSELL, Order::NULL, Order::BUY]);
    }

    #[test]
    fn decimal_conversions() {
        assert!((dec_to_f64(dec!(1.25)) - 1.25).abs() < 1e-12);
        assert_eq!(dec_to_u64(dec!(12.9)), 12);
        assert_eq!(dec_to_u64(dec!(-5)), 0); // clamped non-negative
    }
}
