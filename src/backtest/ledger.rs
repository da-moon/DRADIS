//! W5 — Authoritative Decimal ledger with binary settlement.
//!
//! This is the AUTHORITATIVE PnL view: it prices the actual binary YES/NO shares the
//! vipers trade, settling each open leg at 0/1 against the strike at market close.
//! (The rs-backtester run in `report` is a separate directional proxy.)
//!
//! Per-trade PnL is booked the same way the live bot books it
//! (`pnl = (exit_price − avg_entry) × shares − fees`); the aggregate realized PnL is
//! fed back into `ctx.session_pnl` each tick so drawdown gates see live P&L.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Serialize;
use std::collections::BTreeMap;

/// How a position was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CloseKind {
    /// Strategy-emitted exit, filled at the modeled bid.
    Exit,
    /// Binary settlement at market close (0/1 vs strike).
    Settlement,
}

/// One fully-closed trade (entry + exit or entry + settlement).
#[derive(Debug, Clone, Serialize)]
pub struct ClosedTrade {
    pub strategy: String,
    /// "YES" or "NO".
    pub side: String,
    pub kind: CloseKind,
    pub entry_ts: DateTime<Utc>,
    pub exit_ts: DateTime<Utc>,
    pub entry_price: Decimal,
    pub exit_price: Decimal,
    pub shares: Decimal,
    pub pnl: Decimal,
    pub reason: String,
}

/// Per-strategy roll-up computed at report time.
#[derive(Debug, Clone, Serialize)]
pub struct StrategyStats {
    pub strategy: String,
    pub trades: usize,
    pub wins: usize,
    pub win_rate: f64,
    pub pnl: Decimal,
}

/// Decimal-native ledger: records closed trades, tracks realized PnL, and samples an
/// equity curve (starting + realized + current unrealized mark-to-market).
pub struct Ledger {
    starting: Decimal,
    realized: Decimal,
    closed: Vec<ClosedTrade>,
    equity_curve: Vec<(DateTime<Utc>, Decimal)>,
}

impl Ledger {
    pub fn new(starting: Decimal) -> Self {
        Self {
            starting,
            realized: dec!(0),
            closed: Vec::new(),
            equity_curve: Vec::new(),
        }
    }

    /// Record a closed trade and fold its PnL into the running realized total.
    pub fn record_close(&mut self, trade: ClosedTrade) {
        self.realized += trade.pnl;
        self.closed.push(trade);
    }

    /// Sample the equity curve: `starting + realized + unrealized` at `ts`.
    pub fn push_equity(&mut self, ts: DateTime<Utc>, unrealized: Decimal) {
        self.equity_curve
            .push((ts, self.starting + self.realized + unrealized));
    }

    /// Aggregate realized PnL — fed into `ctx.session_pnl` each tick.
    pub fn realized(&self) -> Decimal {
        self.realized
    }

    pub fn starting(&self) -> Decimal {
        self.starting
    }

    pub fn closed_trades(&self) -> &[ClosedTrade] {
        &self.closed
    }

    pub fn equity_curve(&self) -> &[(DateTime<Utc>, Decimal)] {
        &self.equity_curve
    }

    /// Per-strategy roll-ups, sorted by strategy name.
    pub fn per_strategy(&self) -> Vec<StrategyStats> {
        let mut map: BTreeMap<String, (usize, usize, Decimal)> = BTreeMap::new();
        for t in &self.closed {
            let e = map.entry(t.strategy.clone()).or_insert((0, 0, dec!(0)));
            e.0 += 1;
            if t.pnl > dec!(0) {
                e.1 += 1;
            }
            e.2 += t.pnl;
        }
        map.into_iter()
            .map(|(strategy, (trades, wins, pnl))| StrategyStats {
                strategy,
                trades,
                wins,
                win_rate: if trades > 0 {
                    wins as f64 / trades as f64
                } else {
                    0.0
                },
                pnl,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn trade(strategy: &str, side: &str, entry: &str, exit: &str, shares: &str, kind: CloseKind) -> ClosedTrade {
        let entry_p = Decimal::from_str(entry).unwrap();
        let exit_p = Decimal::from_str(exit).unwrap();
        let sh = Decimal::from_str(shares).unwrap();
        ClosedTrade {
            strategy: strategy.into(),
            side: side.into(),
            kind,
            entry_ts: Utc::now(),
            exit_ts: Utc::now(),
            entry_price: entry_p,
            exit_price: exit_p,
            shares: sh,
            pnl: (exit_p - entry_p) * sh,
            reason: "test".into(),
        }
    }

    #[test]
    fn settlement_math_yes_win_and_loss() {
        let mut l = Ledger::new(dec!(500));
        // YES bought at 0.40, settles at 1.0 → +0.60 * 100 = +60.
        l.record_close(trade("Momentum", "YES", "0.40", "1.0", "100", CloseKind::Settlement));
        // NO bought at 0.55, settles at 0.0 → -0.55 * 50 = -27.5.
        l.record_close(trade("Basis", "NO", "0.55", "0.0", "50", CloseKind::Settlement));
        assert_eq!(l.realized(), dec!(60) - dec!(27.5));
    }

    #[test]
    fn per_strategy_rollup_counts_wins() {
        let mut l = Ledger::new(dec!(500));
        l.record_close(trade("Momentum", "YES", "0.40", "0.50", "10", CloseKind::Exit)); // +1
        l.record_close(trade("Momentum", "YES", "0.60", "0.50", "10", CloseKind::Exit)); // -1
        let stats = l.per_strategy();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].trades, 2);
        assert_eq!(stats[0].wins, 1);
        assert!((stats[0].win_rate - 0.5).abs() < 1e-9);
    }

    #[test]
    fn equity_curve_tracks_starting_plus_realized() {
        let mut l = Ledger::new(dec!(500));
        l.record_close(trade("Momentum", "YES", "0.40", "0.50", "100", CloseKind::Exit)); // +10
        let ts = Utc::now();
        l.push_equity(ts, dec!(5)); // +5 unrealized
        assert_eq!(l.equity_curve().last().unwrap().1, dec!(515)); // 500 + 10 + 5
    }
}
