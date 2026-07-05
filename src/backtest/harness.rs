//! W5 — Replay harness: drives the REAL vipers over synthetic hourly markets and
//! books every fill into the authoritative Decimal ledger.
//!
//! Per replay tick (one 1m candle) the harness:
//!   1. rolls to the active synthetic hourly market (close = top of the next hour,
//!      strike = the oracle at the hour open — the same reference the production
//!      strike derivation in `helpers::time` uses), settling the prior market's open
//!      legs at 0/1 on rotation;
//!   2. synthesizes a `MarketSnapshot` (`synth`), builds a `StrategyContext` with the
//!      W1 clock seam set to HISTORICAL time (`wall_now`/`mono_now` from `ReplayClock`),
//!      a harness-owned `Arc<Mutex<PositionMap>>`, a `DynamicConfig` with the selected
//!      strategy enable flags, and `session_pnl` from the ledger;
//!   3. calls `evaluate_exit` then `evaluate_entry` on every viper (exits prioritised,
//!      mirroring `executor`/`patrol`), filling entries at the modeled ask (capped by
//!      modeled depth) and exits at the modeled bid;
//!   4. records the per-candle net directional stance for the rs-backtester proxy.
//!
//! No CLOB client, wallet, or live DB pool is required — TrendReversal's SQLite
//! cascade guard harmlessly no-ops when no pool is registered.

use std::sync::Arc;

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tokio::sync::Mutex;
use tracing::info;

use crate::helpers::dynamic_config::DynamicConfig;
use crate::helpers::paper::binary_settlement_payout;
use crate::orchestrator::{StrategyContext, StrategyRegistry};
use crate::state::{MarketConfig, MarketSnapshot, OrderParams, Position, PositionMap, StrategySignal};
use crate::venues::core::{MarketId, TimeInForce};

use super::bridge::Stance;
use super::clock::ReplayClock;
use super::fetch::{BacktestCache, Candle, FundingPoint};
use super::ledger::{ClosedTrade, CloseKind, Ledger};
use super::llm_score::{LlmScorer, ScoredEntry};
use super::synth::SnapshotSynthesizer;

const HOUR_MS: i64 = 3_600_000;

/// Fully-resolved backtest parameters (from the CLI).
pub struct BacktestConfig {
    /// Hyperliquid coin symbol, e.g. "BTC".
    pub coin: String,
    /// Lowercase asset used for `ctx.crypto_filter` (BTC-only gates read this).
    pub crypto_filter: String,
    pub interval: String,
    pub start_ms: i64,
    pub end_ms: i64,
    /// `None` → all vipers enabled; `Some(list)` → only those (by short name).
    pub strategies: Option<Vec<String>>,
    pub half_spread: Decimal,
    pub depth: Decimal,
    pub commission: Decimal,
    pub starting_collateral: Decimal,
    pub llm_score: bool,
    pub cache_path: String,
    pub out_dir: String,
    pub sigma_window: usize,
}

/// Everything `report` needs after a run.
pub struct BacktestOutcome {
    pub ledger: Ledger,
    /// Per-candle net directional stance (aligned 1:1 with `candles`) for the proxy.
    pub stances: Vec<Stance>,
    pub candles: Vec<Candle>,
    pub scored_entries: Vec<ScoredEntry>,
    pub ticks: usize,
    pub markets: usize,
}

/// The market currently active during the replay.
struct ActiveMarket {
    open_ms: i64,
    close_wall: DateTime<Utc>,
    strike: Option<Decimal>,
    yes_token: MarketId,
    no_token: MarketId,
    config: MarketConfig,
    started_at: DateTime<Utc>,
}

/// Apply the `--strategies` selection to a `DynamicConfig`.
fn configure_enables(dc: &mut DynamicConfig, sel: &Option<Vec<String>>) {
    let all = sel.is_none();
    let set = |name: &str| -> bool {
        match sel {
            None => true,
            Some(list) => list.iter().any(|s| {
                let s = s.trim().to_ascii_lowercase();
                s == name
            }),
        }
    };
    dc.enable_momentum = all || set("momentum");
    dc.enable_arbitrage = all || set("arbitrage");
    dc.enable_time_decay = all || set("timedecay") || set("time_decay");
    dc.enable_maker = all || set("maker");
    dc.enable_basis = all || set("basis");
    dc.enable_gboost = all || set("gboost");
    dc.enable_trendcapture = all || set("trendreversal") || set("trendcapture");
    dc.enable_convergence = all || set("convergence");
    // Backtest fills are modeled, never placed — mark orders ghost for correctness.
    dc.ghost_mode = true;
}

/// Latest funding rate at or before `ts_ms` (raw HL hourly rate; 0 if none yet).
fn funding_at(funding: &[FundingPoint], ts_ms: i64) -> Decimal {
    match funding.binary_search_by(|f| f.ts_ms.cmp(&ts_ms)) {
        Ok(i) => funding[i].rate,
        Err(0) => dec!(0),
        Err(i) => funding[i - 1].rate,
    }
}

pub async fn run_backtest(cfg: &BacktestConfig) -> Result<BacktestOutcome> {
    let cache = BacktestCache::open(&cfg.cache_path).await?;
    let http = reqwest::Client::new();

    let candles = cache
        .load_candles(&http, &cfg.coin, &cfg.interval, cfg.start_ms, cfg.end_ms)
        .await?;
    if candles.len() < 2 {
        bail!(
            "not enough candles for {} {} in [{}, {}) — got {}",
            cfg.coin,
            cfg.interval,
            cfg.start_ms,
            cfg.end_ms,
            candles.len()
        );
    }
    // ── Coverage check ──────────────────────────────────────────────────────
    // Hyperliquid retains ONLY the most recent ~5000 candles and anchors every
    // candleSnapshot response at the TAIL, so a window older than ~5000×interval is
    // silently head-truncated (the fetch can never fill the missing head — it is
    // purged server-side). Warn loudly when the replayed range does not cover the
    // request; `report.json` records both the requested and the actual replayed range.
    let step_ms = (candles[1].ts_ms - candles[0].ts_ms).max(1);
    let first_ms = candles[0].ts_ms;
    let last_ms = candles.last().unwrap().ts_ms;
    let head_gap = first_ms - cfg.start_ms;
    let tail_gap = cfg.end_ms - (last_ms + step_ms);
    if head_gap > 2 * step_ms || tail_gap > 2 * step_ms {
        tracing::warn!(
            "⚠️  requested [{}, {}) but replay only covers [{}, {}]: ~{} min head / ~{} min tail \
             uncovered. Hyperliquid retains only the most recent ~5000 candles and anchors \
             responses at the tail, so windows older than ~5000×interval are truncated. The \
             summary/report start/end reflect the REQUEST; see replayed_start_ms/replayed_end_ms \
             for what was actually replayed.",
            cfg.start_ms, cfg.end_ms, first_ms, last_ms,
            head_gap.max(0) / 60_000, tail_gap.max(0) / 60_000,
        );
    }

    let funding = cache
        .load_funding(&http, &cfg.coin, cfg.start_ms, cfg.end_ms)
        .await?;
    info!(
        "▶️  replay: {} candles, {} funding points [{}]",
        candles.len(),
        funding.len(),
        cfg.coin
    );

    // Price lookup: greatest candle close with ts <= query (nearest-prior).
    let ts_sorted: Vec<i64> = candles.iter().map(|c| c.ts_ms).collect();
    let price_at_or_before = |ts: i64| -> Option<Decimal> {
        match ts_sorted.binary_search(&ts) {
            Ok(i) => Some(candles[i].close),
            Err(0) => None,
            Err(i) => Some(candles[i - 1].close),
        }
    };

    let clock = ReplayClock::new(candles[0].ts_ms);
    let mut synth = SnapshotSynthesizer::new(cfg.half_spread, cfg.depth, cfg.sigma_window);
    let mut ledger = Ledger::new(cfg.starting_collateral);
    let positions: Arc<Mutex<PositionMap>> = Arc::new(Mutex::new(PositionMap::new()));

    let mut base_dc = DynamicConfig::default();
    configure_enables(&mut base_dc, &cfg.strategies);
    let dc = Arc::new(base_dc);

    let strategies = StrategyRegistry::create_all_strategies();

    // Optional experimental LLM decision scorer (shares the cache DB).
    let mut scorer = if cfg.llm_score {
        match LlmScorer::new(cache.pool().clone()).await {
            Ok(s) => Some(s),
            Err(e) => {
                info!("🤖 LLM scoring disabled: {e}");
                None
            }
        }
    } else {
        None
    };
    let mut scored_entries: Vec<ScoredEntry> = Vec::new();

    let mut cur: Option<ActiveMarket> = None;
    let mut stances: Vec<Stance> = Vec::with_capacity(candles.len());
    let mut markets_seen = 0usize;

    for candle in &candles {
        let ts = candle.ts_ms;
        let open_ms = (ts / HOUR_MS) * HOUR_MS;

        // ── Rotate market on hour boundary (settle the prior market first) ──────
        if cur.as_ref().map(|m| m.open_ms) != Some(open_ms) {
            if let Some(prev) = cur.take() {
                settle_market(&prev, &price_at_or_before, &positions, &mut ledger, cfg.commission).await;
            }
            let close_ms = open_ms + HOUR_MS;
            let close_wall = clock.wall(close_ms);
            let strike = price_at_or_before(open_ms);
            let yes_token = MarketId::new(format!("{}-{}-YES", cfg.coin, open_ms));
            let no_token = MarketId::new(format!("{}-{}-NO", cfg.coin, open_ms));
            let condition_id = format!("{}-{}", cfg.coin, open_ms);
            let market_name = format!("{} Up or Down (bt {})", cfg.coin, clock.wall(open_ms));
            let config = MarketConfig {
                yes_token: yes_token.clone(),
                no_token: no_token.clone(),
                market_name,
                market_close_time: Some(close_wall),
                strike_price: strike,
                is_neg_risk: false,
                condition_id,
                yes_fee_bps: 0,
                no_fee_bps: 0,
            };
            cur = Some(ActiveMarket {
                open_ms,
                close_wall,
                strike,
                yes_token,
                no_token,
                config,
                started_at: clock.wall(open_ms),
            });
            markets_seen += 1;
        }
        let market = cur.as_ref().unwrap();

        // ── Synthesize snapshot + context ───────────────────────────────────────
        let funding_rate = funding_at(&funding, ts);
        let snapshot = synth.on_tick(&clock, candle, market.close_wall, market.strike, funding_rate);
        let realized = ledger.realized();
        let ctx = StrategyContext {
            market: market.config.clone(),
            snapshot: snapshot.clone(),
            positions: Arc::clone(&positions),
            session_pnl: realized,
            starting_collateral: cfg.starting_collateral,
            crypto_filter: cfg.crypto_filter.clone(),
            market_started_at: market.started_at,
            maker_market: None,
            maker_snapshot: None,
            available_collateral: cfg.starting_collateral + realized,
            dynamic_config: Arc::clone(&dc),
            arb_market_lockouts: None,
            wall_now: clock.wall(ts),
            mono_now: clock.mono_std(ts),
            // Replay run — vipers must NOT consult the live bot's persistent SQLite
            // state (e.g. TrendReversal's cascade guard). See StrategyContext::is_replay.
            is_replay: true,
        };

        // ── Exits first, then entries (mirror executor priority) ────────────────
        for strat in &strategies {
            if let Ok(sig) = strat.evaluate_exit(&ctx).await {
                apply_exit(&strat.name(), &sig, &snapshot, market, &positions, &mut ledger, cfg.commission).await;
            }
        }
        for strat in &strategies {
            if let Ok(sig) = strat.evaluate_entry(&ctx).await {
                apply_entry(
                    &strat.name(),
                    &sig,
                    &snapshot,
                    market,
                    &positions,
                    ctx.wall_now,
                    cfg.commission,
                    scorer.as_mut(),
                    &mut scored_entries,
                )
                .await;
            }
        }

        // ── Equity curve + rs-backtester stance ─────────────────────────────────
        let (unrealized, stance) = mark_positions(&positions, &snapshot, market).await;
        ledger.push_equity(clock.wall(ts), unrealized);
        stances.push(stance);
    }

    // Settle the final open market at the last candle's close.
    if let Some(prev) = cur.take() {
        settle_market(&prev, &price_at_or_before, &positions, &mut ledger, cfg.commission).await;
    }

    // Attach realized outcomes to any LLM-scored entries.
    join_scores(&mut scored_entries, &ledger);

    Ok(BacktestOutcome {
        ledger,
        stances,
        candles: candles.clone(),
        scored_entries,
        ticks: candles.len(),
        markets: markets_seen,
    })
}

/// Resolve which side (`Some(true)`=YES / `Some(false)`=NO) a token trades, on the
/// active synthetic market.
fn side_of(token: &MarketId, m: &ActiveMarket) -> Option<bool> {
    if token == &m.yes_token {
        Some(true)
    } else if token == &m.no_token {
        Some(false)
    } else {
        None
    }
}

/// Fill one entry leg at the modeled ask (capped by modeled depth), updating the
/// position map. Optionally scores the decision with the LLM.
#[allow(clippy::too_many_arguments)]
async fn apply_entry(
    strategy: &str,
    sig: &StrategySignal,
    snap: &MarketSnapshot,
    m: &ActiveMarket,
    positions: &Arc<Mutex<PositionMap>>,
    wall_now: DateTime<Utc>,
    commission: Decimal,
    scorer: Option<&mut LlmScorer>,
    scored: &mut Vec<ScoredEntry>,
) {
    let StrategySignal::Entry { params, pair_params } = sig else {
        return;
    };
    let mut legs: Vec<&OrderParams> = vec![params];
    if let Some(p) = pair_params {
        legs.push(p);
    }

    // LLM scoring: one score per Entry signal, keyed to the primary leg.
    if let Some(sc) = scorer {
        if let Some(is_yes) = side_of(&params.token_id, m) {
            if let Some(entry) = sc
                .score_entry(strategy, is_yes, wall_now, snap, params)
                .await
            {
                scored.push(entry);
            }
        }
    }

    let mut map = positions.lock().await;
    for leg in legs {
        let Some(is_yes) = side_of(&leg.token_id, m) else {
            continue;
        };
        // Honor the order's resting semantics. A `post_only` / GTC-GTD limit is a
        // resting MAKER bid: it fills at its OWN posted limit price when that price
        // sits at/above the modeled best bid (it would be the front of the book and
        // get hit) — filling it at the ask like a taker inverts maker economics
        // (e.g. TimeDecay's 1−(yes_bid+no_bid) edge would flip to a guaranteed loss).
        // Everything else (FAK/FOK, or a crossing limit) is a taker and fills at the ask.
        let is_maker =
            leg.post_only || matches!(leg.order_type, TimeInForce::Gtc | TimeInForce::Gtd);
        let (fill_price, depth) = if is_maker {
            let (bid, bid_depth) = if is_yes {
                (snap.yes_bid, snap.yes_bid_depth)
            } else {
                (snap.no_bid, snap.no_bid_depth)
            };
            // Limit below the best bid would not be hit this tick — no fill.
            if leg.price < bid {
                continue;
            }
            (leg.price, bid_depth)
        } else if is_yes {
            (snap.yes_ask, snap.yes_ask_depth)
        } else {
            (snap.no_ask, snap.no_ask_depth)
        };
        if fill_price <= dec!(0) {
            continue;
        }
        let fill = leg.shares.min(depth);
        if fill <= dec!(0) {
            continue;
        }
        let key = (strategy.to_string(), leg.token_id.clone());
        let entry = map.entry(key).or_insert_with(|| Position {
            shares: dec!(0),
            avg_entry: fill_price,
            opened_at: wall_now,
            close_time: m.config.market_close_time,
            market_name: m.config.market_name.clone(),
            pair_token_id: leg.token_id.clone(),
            // Fills are deterministic in replay — confirm immediately so exit gates engage.
            fill_confirmed_at: Some(wall_now),
            paired_leg_token_id: None,
            ghost: true,
        });
        let new_shares = entry.shares + fill;
        if new_shares > dec!(0) {
            entry.avg_entry = (entry.avg_entry * entry.shares + fill_price * fill) / new_shares;
        }
        entry.shares = new_shares;
    }
    let _ = commission; // entry-side fee folded into exit/settlement PnL below.
}

/// Fill an exit at the modeled bid; on `exit_pair`, also close the strategy's other
/// leg on this market.
async fn apply_exit(
    strategy: &str,
    sig: &StrategySignal,
    snap: &MarketSnapshot,
    m: &ActiveMarket,
    positions: &Arc<Mutex<PositionMap>>,
    ledger: &mut Ledger,
    commission: Decimal,
) {
    let StrategySignal::Exit { params, reason, exit_pair } = sig else {
        return;
    };
    let mut tokens: Vec<MarketId> = vec![params.token_id.clone()];
    if *exit_pair {
        // Close both legs of the pair for this strategy on this market.
        for t in [&m.yes_token, &m.no_token] {
            if !tokens.contains(t) {
                tokens.push(t.clone());
            }
        }
    }

    let mut map = positions.lock().await;
    for token in tokens {
        let Some(is_yes) = side_of(&token, m) else {
            continue;
        };
        let bid = if is_yes { snap.yes_bid } else { snap.no_bid };
        let bid_depth = if is_yes { snap.yes_bid_depth } else { snap.no_bid_depth };
        let key = (strategy.to_string(), token.clone());
        let Some(pos) = map.get(&key).cloned() else {
            continue;
        };
        if pos.shares <= dec!(0) {
            continue;
        }
        // Vipers request a full-position exit; cap the fill by modeled bid depth.
        let sell = pos.shares.min(bid_depth);
        if sell <= dec!(0) {
            continue;
        }
        let fees = (pos.avg_entry + bid) * sell * commission;
        let pnl = (bid - pos.avg_entry) * sell - fees;
        ledger.record_close(ClosedTrade {
            strategy: strategy.to_string(),
            side: if is_yes { "YES" } else { "NO" }.to_string(),
            kind: CloseKind::Exit,
            entry_ts: pos.opened_at,
            exit_ts: snap.timestamp,
            entry_price: pos.avg_entry,
            exit_price: bid,
            shares: sell,
            pnl,
            reason: reason.clone(),
        });
        if sell >= pos.shares {
            map.remove(&key);
        } else if let Some(p) = map.get_mut(&key) {
            p.shares -= sell;
        }
    }
}

/// Settle every open leg of a closing market at 0/1 vs strike.
async fn settle_market<F>(
    m: &ActiveMarket,
    price_at_or_before: &F,
    positions: &Arc<Mutex<PositionMap>>,
    ledger: &mut Ledger,
    commission: Decimal,
) where
    F: Fn(i64) -> Option<Decimal>,
{
    let close_ms = m.open_ms + HOUR_MS;
    let settle_price = match price_at_or_before(close_ms) {
        Some(p) => p,
        None => return,
    };
    let strike = m.strike;
    let mut map = positions.lock().await;
    let keys: Vec<(String, MarketId)> = map
        .keys()
        .filter(|(_, t)| t == &m.yes_token || t == &m.no_token)
        .cloned()
        .collect();
    for key in keys {
        let Some(pos) = map.remove(&key) else { continue };
        if pos.shares <= dec!(0) {
            continue;
        }
        let is_yes = key.1 == m.yes_token;
        // Match the live paper-trading / viper convention exactly (helpers::paper):
        // `oracle >= strike ⇒ YES wins` (a tie pays YES), and an UNKNOWN strike is
        // unresolvable so BOTH sides pay $0 — never a free NO win.
        let payout = binary_settlement_payout(is_yes, settle_price, strike);
        // Charge the entry-side fee deferred by `apply_entry` (redemption itself is
        // fee-free on Polymarket), so settlement-closed legs are not silently cheaper
        // than exit-closed legs under a nonzero `--commission`.
        let fees = pos.avg_entry * pos.shares * commission;
        let pnl = (payout - pos.avg_entry) * pos.shares - fees;
        ledger.record_close(ClosedTrade {
            strategy: key.0.clone(),
            side: if is_yes { "YES" } else { "NO" }.to_string(),
            kind: CloseKind::Settlement,
            entry_ts: pos.opened_at,
            exit_ts: m.close_wall,
            entry_price: pos.avg_entry,
            exit_price: payout,
            shares: pos.shares,
            pnl,
            reason: format!("Settlement (final=${} strike={:?})", settle_price, strike),
        });
    }
}

/// Mark-to-market open positions on the active market: returns (unrealized PnL,
/// net directional stance for the rs-backtester proxy).
async fn mark_positions(
    positions: &Arc<Mutex<PositionMap>>,
    snap: &MarketSnapshot,
    m: &ActiveMarket,
) -> (Decimal, Stance) {
    let map = positions.lock().await;
    let mut unrealized = dec!(0);
    let mut yes_notional = dec!(0);
    let mut no_notional = dec!(0);
    for ((_, token), pos) in map.iter() {
        let Some(is_yes) = side_of(token, m) else { continue };
        let bid = if is_yes { snap.yes_bid } else { snap.no_bid };
        unrealized += (bid - pos.avg_entry) * pos.shares;
        let notional = pos.avg_entry * pos.shares;
        if is_yes {
            yes_notional += notional;
        } else {
            no_notional += notional;
        }
    }
    let stance = if yes_notional > no_notional {
        Stance::Long
    } else if no_notional > yes_notional {
        Stance::Short
    } else {
        Stance::Flat
    };
    (unrealized, stance)
}

/// Attach each scored entry's realized outcome by matching the nearest closed trade
/// with the same strategy+side and entry timestamp.
fn join_scores(scored: &mut [ScoredEntry], ledger: &Ledger) {
    let closed = ledger.closed_trades();
    for s in scored.iter_mut() {
        let mut best: Option<(&ClosedTrade, i64)> = None;
        for t in closed {
            if t.strategy == s.strategy && t.side == s.side {
                let dist = (t.entry_ts - s.entry_ts).num_seconds().abs();
                if best.map(|(_, d)| dist < d).unwrap_or(true) {
                    best = Some((t, dist));
                }
            }
        }
        if let Some((t, dist)) = best {
            if dist <= 120 {
                s.realized_pnl = Some(t.pnl);
            }
        }
    }
}
