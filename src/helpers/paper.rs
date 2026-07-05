//! Pure math for the paper-trading (ghost) simulation.
//!
//! These helpers back the depth-aware simulated fills (D4), the resting-maker-quote
//! fill heuristic (D5), and the binary expiry settlement payout (D6). They are kept
//! free of any I/O or shared state so the fill/settlement logic can be unit-tested
//! in isolation; the patrol loop and cleanup task supply the live inputs.

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Weighted-average fill price for a simulated (ghost) taker BUY of `shares`.
///
/// Up to `depth` shares fill at `base_price` (the strategy's requested price with
/// the usual entry offsets already applied); any excess fills one tranche worse, at
/// `base_price + overflow_slippage`, capped at `price_cap`. The two tranches are
/// weighted-averaged into a single effective entry price.
///
/// `depth <= 0` means no visible top-of-book liquidity, so the whole order fills in
/// the overflow tranche. `shares <= 0` returns `base_price` (no fill to average).
pub fn simulate_taker_buy_avg(
    base_price: Decimal,
    depth: Decimal,
    shares: Decimal,
    overflow_slippage: Decimal,
    price_cap: Decimal,
) -> Decimal {
    if shares <= dec!(0) {
        return base_price;
    }
    let at_base = shares.min(depth.max(dec!(0)));
    let overflow = (shares - at_base).max(dec!(0));
    let overflow_price = (base_price + overflow_slippage).min(price_cap);
    (at_base * base_price + overflow * overflow_price) / shares
}

/// Weighted-average fill price for a simulated (ghost) taker SELL of `shares`.
///
/// Symmetric to [`simulate_taker_buy_avg`]: up to `depth` shares fill at `base_price`
/// (the current bid with the usual exit offset already applied); any excess fills one
/// tranche worse, at `base_price - overflow_slippage`, floored at `price_floor`.
pub fn simulate_taker_sell_avg(
    base_price: Decimal,
    depth: Decimal,
    shares: Decimal,
    overflow_slippage: Decimal,
    price_floor: Decimal,
) -> Decimal {
    if shares <= dec!(0) {
        return base_price;
    }
    let at_base = shares.min(depth.max(dec!(0)));
    let overflow = (shares - at_base).max(dec!(0));
    let overflow_price = (base_price - overflow_slippage).max(price_floor);
    (at_base * base_price + overflow * overflow_price) / shares
}

/// Advance the consecutive-tick counter for a resting ghost maker BUY quote.
///
/// A resting BUY at `quote_price` is "in the money to be hit" while the market's
/// best bid has fallen to at or below it. Each such tick increments the counter;
/// any tick where the best bid is above the quote resets it to zero.
pub fn maker_next_tick_count(prev: u64, best_bid: Decimal, quote_price: Decimal) -> u64 {
    if best_bid <= quote_price { prev + 1 } else { 0 }
}

/// Whether a resting ghost maker quote has rested long enough to be treated as filled.
pub fn maker_should_fill(consecutive_ticks: u64, threshold: u64) -> bool {
    consecutive_ticks >= threshold
}

/// Binary per-share settlement payout for a ghost position at expiry.
///
/// A YES token pays $1 when the oracle resolves at or above the strike (the "up"
/// outcome), $0 otherwise; a NO token is the mirror. Matches the momentum/trend
/// vipers' `oracle_price >= strike ⇒ YES` convention. When the strike is unknown
/// the market cannot be resolved from the oracle, so the payout is $0.
pub fn binary_settlement_payout(
    is_yes_token: bool,
    oracle_price: Decimal,
    strike: Option<Decimal>,
) -> Decimal {
    match strike {
        Some(k) => {
            let yes_wins = oracle_price >= k;
            let token_wins = if is_yes_token { yes_wins } else { !yes_wins };
            if token_wins { dec!(1) } else { dec!(0) }
        }
        None => dec!(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLIP: Decimal = dec!(0.02);
    const CAP: Decimal = dec!(0.97);
    const FLOOR: Decimal = dec!(0.01);

    #[test]
    fn buy_fully_within_depth_has_no_slippage() {
        // 5 shares, 10 available at base → all fill at base, no overflow tranche.
        let avg = simulate_taker_buy_avg(dec!(0.50), dec!(10), dec!(5), SLIP, CAP);
        assert_eq!(avg, dec!(0.50));
    }

    #[test]
    fn buy_overflow_is_weighted_averaged() {
        // 10 shares, only 4 at base(0.50); 6 overflow at 0.52.
        // avg = (4*0.50 + 6*0.52) / 10 = (2.00 + 3.12)/10 = 0.512
        let avg = simulate_taker_buy_avg(dec!(0.50), dec!(4), dec!(10), SLIP, CAP);
        assert_eq!(avg, dec!(0.512));
    }

    #[test]
    fn buy_zero_depth_fills_entirely_in_overflow() {
        let avg = simulate_taker_buy_avg(dec!(0.50), dec!(0), dec!(8), SLIP, CAP);
        assert_eq!(avg, dec!(0.52));
    }

    #[test]
    fn buy_overflow_price_respects_cap() {
        // base 0.96 + 0.02 = 0.98 but capped at 0.97.
        let avg = simulate_taker_buy_avg(dec!(0.96), dec!(0), dec!(1), SLIP, CAP);
        assert_eq!(avg, dec!(0.97));
    }

    #[test]
    fn sell_overflow_is_weighted_averaged() {
        // 10 shares, 4 at base(0.50); 6 overflow at 0.48.
        // avg = (4*0.50 + 6*0.48)/10 = (2.00 + 2.88)/10 = 0.488
        let avg = simulate_taker_sell_avg(dec!(0.50), dec!(4), dec!(10), SLIP, FLOOR);
        assert_eq!(avg, dec!(0.488));
    }

    #[test]
    fn sell_overflow_price_respects_floor() {
        // base 0.02 - 0.02 = 0.00 but floored at 0.01.
        let avg = simulate_taker_sell_avg(dec!(0.02), dec!(0), dec!(1), SLIP, FLOOR);
        assert_eq!(avg, dec!(0.01));
    }

    #[test]
    fn zero_shares_returns_base() {
        assert_eq!(simulate_taker_buy_avg(dec!(0.5), dec!(3), dec!(0), SLIP, CAP), dec!(0.5));
        assert_eq!(simulate_taker_sell_avg(dec!(0.5), dec!(3), dec!(0), SLIP, FLOOR), dec!(0.5));
    }

    #[test]
    fn maker_tick_counting_and_fill() {
        let threshold = 3u64;
        let quote = dec!(0.40);
        // best bid above quote → resets.
        let mut c = maker_next_tick_count(5, dec!(0.45), quote);
        assert_eq!(c, 0);
        assert!(!maker_should_fill(c, threshold));
        // best bid <= quote for 3 consecutive ticks → fills.
        c = maker_next_tick_count(c, dec!(0.40), quote); // 1
        assert!(!maker_should_fill(c, threshold));
        c = maker_next_tick_count(c, dec!(0.39), quote); // 2
        assert!(!maker_should_fill(c, threshold));
        c = maker_next_tick_count(c, dec!(0.38), quote); // 3
        assert_eq!(c, 3);
        assert!(maker_should_fill(c, threshold));
    }

    #[test]
    fn settlement_payout_yes_wins_above_strike() {
        let strike = Some(dec!(100000));
        // Oracle above strike → YES pays $1, NO pays $0.
        assert_eq!(binary_settlement_payout(true, dec!(100050), strike), dec!(1));
        assert_eq!(binary_settlement_payout(false, dec!(100050), strike), dec!(0));
    }

    #[test]
    fn settlement_payout_no_wins_below_strike() {
        let strike = Some(dec!(100000));
        // Oracle below strike → YES pays $0, NO pays $1.
        assert_eq!(binary_settlement_payout(true, dec!(99950), strike), dec!(0));
        assert_eq!(binary_settlement_payout(false, dec!(99950), strike), dec!(1));
    }

    #[test]
    fn settlement_payout_exactly_at_strike_is_yes() {
        let strike = Some(dec!(100000));
        assert_eq!(binary_settlement_payout(true, dec!(100000), strike), dec!(1));
        assert_eq!(binary_settlement_payout(false, dec!(100000), strike), dec!(0));
    }

    #[test]
    fn settlement_payout_unknown_strike_is_zero() {
        assert_eq!(binary_settlement_payout(true, dec!(100000), None), dec!(0));
        assert_eq!(binary_settlement_payout(false, dec!(100000), None), dec!(0));
    }
}
