//! Hyperliquid Raptor — one WebSocket task per asset, feeding EVERY raptor
//! channel from a single Hyperliquid Info-API connection.
//!
//! Where the Binance source spawns three tasks (price WS + funding REST + OI/CVD
//! REST), Hyperliquid multiplexes the same signals over one WS connection:
//!
//! │ HL subscription     │ Feeds                                                  │
//! │─────────────────────│────────────────────────────────────────────────────────│
//! │ `Trades{coin}`      │ oracle (last trade px), velocity/accel/drift, taker CVD │
//! │ `ActiveAssetCtx{c}` │ funding rate (×8 normalized), open interest → OI delta  │
//!
//! The derived signals are byte-compatible with the Binance raptors — same
//! `watch` channels, same `PriceKinematics` math, same `DerivativesSnapshot`
//! shape — so `SquadronRaptors`, the vipers, and the telemetry surface need zero
//! changes when `MARKET_DATA_SOURCE=hyperliquid`.
//!
//! Self-healing mirrors `price.rs`: a 30s recv timeout and an independent 60s
//! "no parsed trade" zombie guard both force our own outer rebuild loop
//! (drop the `InfoClient`, build a fresh one) with a 5s backoff. We do NOT rely
//! on the SDK `WsManager`'s internal auto-reconnect for liveness — raptor
//! self-healing must not depend on unverified SDK behaviour.

use std::collections::{HashMap, VecDeque};
use std::str::FromStr;
use std::sync::Arc;

use hyperliquid_rust_sdk::{AssetCtx, BaseUrl, InfoClient, Message, Subscription};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tokio::sync::{mpsc, watch};
use tokio::time::{timeout as tokio_timeout, Duration, Instant};
use tracing::{debug, info, warn};

use crate::api::server::AssetRaptorHealth;
use crate::config;
use crate::helpers::volatility::normalized_hist_vol;
use crate::raptors::derivatives::DerivativesSnapshot;
use crate::raptors::kinematics::PriceKinematics;
use crate::raptors::source;

/// Rolling taker-volume window (5 minutes) used to derive the CVD ratio.
const CVD_WINDOW_SECS: u64 = 300;

/// One raptor task per asset. Feeds oracle/velocity/drift/funding/derivatives
/// channels + the shared `AssetRaptorHealth` map, exactly as the trio of
/// Binance raptors would for the same asset.
pub async fn run_hyperliquid_raptor(
    crypto_filter: String,
    oracle_tx: watch::Sender<Decimal>,
    velocity_tx: watch::Sender<(Decimal, Decimal, Decimal)>,
    // Sends (drift_60m, drift_10m, hist_vol) — matches the Binance price raptor's
    // 3-tuple drift contract (hist_vol is the normalized [0,1] 60-min realized vol).
    drift_tx: watch::Sender<(Decimal, Decimal, Decimal)>,
    funding_tx: watch::Sender<Decimal>,
    deriv_tx: watch::Sender<DerivativesSnapshot>,
    raptor_health_tx: Arc<watch::Sender<HashMap<String, AssetRaptorHealth>>>,
) {
    let coin = source::hyperliquid_coin(&crypto_filter);
    let poll_interval = Duration::from_secs(config::DERIVATIVES_POLL_SECS);

    // ── State that persists across reconnects ────────────────────────────────
    // Momentum/drift accumulator — same math and reconnect semantics as price.rs.
    let mut kin = PriceKinematics::new();
    // Rolling 5m taker volume, split by aggressor side, for the CVD ratio.
    let mut buy_vol: VecDeque<(Instant, Decimal)> = VecDeque::new();
    let mut sell_vol: VecDeque<(Instant, Decimal)> = VecDeque::new();
    // Last emitted CVD ratio. Seeded to 1.0 (neutral) to match Binance's
    // `buySellRatio` semantics; preserved when the sell window is empty.
    let mut cvd_ratio = dec!(1);
    // Open-interest delta bookkeeping — sampled on the DERIVATIVES_POLL_SECS
    // cadence so the 30s-delta semantics the vipers are tuned on are preserved,
    // even though ActiveAssetCtx pushes far more frequently.
    let mut prev_oi: Option<Decimal> = None;
    let mut last_oi_sample: Option<Instant> = None;

    loop {
        // ── (Re)build the client + subscriptions ─────────────────────────────
        let mut client = match InfoClient::new(None, Some(BaseUrl::Mainnet)).await {
            Ok(c) => c,
            Err(e) => {
                warn!("⚠️ Hyperliquid Raptor: failed to build InfoClient for {}: {} — retrying in 5s", coin, e);
                mark_offline(&raptor_health_tx, &crypto_filter);
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        let sub_ok = client
            .subscribe(Subscription::Trades { coin: coin.clone() }, tx.clone())
            .await
            .is_ok()
            && client
                .subscribe(Subscription::ActiveAssetCtx { coin: coin.clone() }, tx.clone())
                .await
                .is_ok();
        // Our local senders are only needed to hand to the SDK; drop them so `rx`
        // closes (→ recv returns None) once the WsManager drops its own senders.
        drop(tx);

        if !sub_ok {
            warn!("⚠️ Hyperliquid Raptor: subscribe failed for {} — reconnecting in 5s", coin);
            mark_offline(&raptor_health_tx, &crypto_filter);
            drop(client);
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }

        info!(" Hyperliquid Raptor connected for {} ({})", coin, crypto_filter.to_uppercase());

        // `last_trade` tracks the last time we parsed an actual trade. ctx frames
        // keep arriving and reset the 30s recv timeout, so — like price.rs — we
        // need an independent 60s guard to catch a "zombie" feed where ctx is
        // alive but the trade stream has silently stopped.
        let mut last_trade = Instant::now();
        // `last_ctx` is the mirror guard for the assetCtx stream: trades alone
        // keep the socket "alive", so a stalled ctx subscription would silently
        // freeze funding/OI/CVD at their last values (Binance can't do this —
        // its funding/derivatives pollers fail loudly on their own). ctx pushes
        // ~1/s, so 90s means dozens of missed frames.
        let mut last_ctx = Instant::now();
        // Kinematics are sampled at ~1 Hz with the latest trade price, matching
        // the Binance @ticker cadence price.rs sees. Feeding every raw trade
        // (dozens/s) would collapse `acceleration` — a per-sample derivative —
        // into inter-trade bid/ask-bounce noise and neuter the Momentum viper's
        // accel gate. ctx frames (~1/s) keep the sampler ticking through trade
        // gaps so velocity decays exactly like it does on Binance's always-on
        // ticker.
        let mut latest_px: Option<Decimal> = None;
        let mut last_kin_feed: Option<Instant> = None;

        'ws: loop {
            // Zombie guard: no parsed trade in 60s → force reconnect.
            if last_trade.elapsed() >= Duration::from_secs(60) {
                warn!("⚠️ Hyperliquid Raptor {}: no trade in 60s (zombie WS — ctx alive but trades silent) — reconnecting", coin);
                break 'ws;
            }
            // Ctx staleness guard: no assetCtx frame in 90s → funding/OI would
            // go stale while trades keep flowing; force reconnect instead.
            if last_ctx.elapsed() >= Duration::from_secs(90) {
                warn!("⚠️ Hyperliquid Raptor {}: no assetCtx in 90s (funding/OI stale) — reconnecting", coin);
                break 'ws;
            }
            // 1 Hz kinematics feed from the latest trade price (see note above).
            if let Some(px) = latest_px {
                if last_kin_feed.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
                    let now = Instant::now();
                    last_kin_feed = Some(now);
                    let sig = kin.on_price(now, px);
                    let _ = velocity_tx.send((sig.velocity_5s, sig.velocity_1s, sig.acceleration));
                    // Match the Binance raptor's 3-tuple drift contract (adds hist_vol).
                    let hist_vol_norm = {
                        let prices = kin.prices_60m_f64();
                        Decimal::from_f64_retain(normalized_hist_vol(&prices)).unwrap_or(Decimal::ZERO)
                    };
                    let _ = drift_tx.send((sig.drift_60m, sig.drift_10m, hist_vol_norm));
                    raptor_health_tx.send_modify(|map| {
                        let h = map.entry(crypto_filter.clone()).or_default();
                        h.velocity_5s = sig.velocity_5s;
                        h.velocity_1s = sig.velocity_1s;
                        h.acceleration = sig.acceleration;
                        h.drift_60m = sig.drift_60m;
                        h.drift_10m = sig.drift_10m;
                    });
                }
            }

            match tokio_timeout(Duration::from_secs(30), rx.recv()).await {
                Ok(Some(msg)) => match msg {
                    Message::Trades(trades) => {
                        for trade in trades.data {
                            let (Ok(px), Ok(sz)) = (
                                Decimal::from_str(&trade.px),
                                Decimal::from_str(&trade.sz),
                            ) else {
                                continue;
                            };
                            let now = Instant::now();
                            last_trade = now;

                            // Oracle = last trade price (absolute value, safe to
                            // refresh per trade). Velocity/accel/drift are fed at
                            // 1 Hz by the loop-top sampler, NOT per trade.
                            latest_px = Some(px);
                            let _ = oracle_tx.send(px);

                            // Taker CVD: HL trade `side` is the aggressor side —
                            // "B" = buy (lifted the ask), "A" = sell (hit the bid).
                            match trade.side.as_str() {
                                "B" => buy_vol.push_back((now, sz)),
                                "A" => sell_vol.push_back((now, sz)),
                                _ => {}
                            }
                            trim_window(&mut buy_vol, now);
                            trim_window(&mut sell_vol, now);

                            raptor_health_tx.send_modify(|map| {
                                let h = map.entry(crypto_filter.clone()).or_default();
                                h.price_connected = true;
                                h.oracle_price = px;
                            });
                        }
                    }
                    Message::ActiveAssetCtx(actx) => {
                        last_ctx = Instant::now();
                        // Only perp contexts carry funding + open interest.
                        if let AssetCtx::Perps(perp) = actx.data.ctx {
                            // Funding normalization: Hyperliquid quotes an HOURLY
                            // funding rate, whereas Binance `lastFundingRate` is
                            // per-8h. The BASIS_*_FUNDING_THRESHOLD viper gates were
                            // tuned on 8h rates, so emit HL funding × 8 to preserve
                            // their meaning across sources.
                            if let Ok(hourly) = Decimal::from_str(&perp.funding) {
                                let normalized = normalize_funding_8h(hourly);
                                let _ = funding_tx.send(normalized);
                                raptor_health_tx.send_modify(|map| {
                                    let h = map.entry(crypto_filter.clone()).or_default();
                                    h.funding_connected = true;
                                    h.funding_rate = normalized;
                                });
                                debug!("📡 Hyperliquid funding {}: {:.6}% (×8 of hourly)", coin, normalized * dec!(100));
                            }

                            // Open interest → sampled OI delta on the poll cadence.
                            if let Ok(open_interest) = Decimal::from_str(&perp.open_interest) {
                                let now = Instant::now();
                                let due = last_oi_sample.is_none_or(|t| t.elapsed() >= poll_interval);
                                if due {
                                    let oi_delta_pct = match prev_oi {
                                        Some(p) if p > dec!(0) => (open_interest - p) / p,
                                        _ => dec!(0),
                                    };
                                    prev_oi = Some(open_interest);
                                    last_oi_sample = Some(now);

                                    // Recompute CVD ratio from the rolling windows.
                                    // Zero sell volume → keep the previous ratio.
                                    let buy_sum: Decimal = buy_vol.iter().map(|(_, v)| *v).sum();
                                    let sell_sum: Decimal = sell_vol.iter().map(|(_, v)| *v).sum();
                                    cvd_ratio = cvd_ratio_or_prev(buy_sum, sell_sum, cvd_ratio);

                                    let snap = DerivativesSnapshot { open_interest, oi_delta_pct, cvd_ratio };
                                    let _ = deriv_tx.send(snap);
                                    raptor_health_tx.send_modify(|map| {
                                        let h = map.entry(crypto_filter.clone()).or_default();
                                        h.deriv_connected = true;
                                        h.open_interest = open_interest;
                                        h.oi_delta_pct = oi_delta_pct;
                                        h.cvd_ratio = cvd_ratio;
                                    });
                                    debug!(
                                        "📡 Hyperliquid derivatives {}: OI={} ΔOI={:.3}% CVD={:.3}",
                                        coin, open_interest, oi_delta_pct * dec!(100), cvd_ratio,
                                    );
                                }
                            }
                        }
                    }
                    // WsManager pushes NoData when the socket drops — treat as a
                    // disconnect and rebuild via our own outer loop.
                    Message::NoData => {
                        warn!("⚠️ Hyperliquid Raptor {}: WS closed by server — reconnecting", coin);
                        break 'ws;
                    }
                    Message::HyperliquidError(err) => {
                        warn!("⚠️ Hyperliquid Raptor {}: server error: {}", coin, err);
                    }
                    // Pong / SubscriptionResponse / other channels — ignore.
                    _ => {}
                },
                // All senders dropped → the WsManager reader stopped; reconnect.
                Ok(None) => break 'ws,
                // 30s with no message at all — silent stall; force reconnect.
                Err(_) => {
                    warn!("⚠️ Hyperliquid Raptor {}: no message in 30s — reconnecting", coin);
                    break 'ws;
                }
            }
        }

        warn!("⚠️ Hyperliquid Raptor {} disconnected. Reconnecting in 5s...", coin);
        mark_offline(&raptor_health_tx, &crypto_filter);
        kin.reset_velocity();
        drop(client);
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Drop taker-volume entries older than the 5-minute CVD window.
fn trim_window(window: &mut VecDeque<(Instant, Decimal)>, now: Instant) {
    while let Some((t, _)) = window.front() {
        if now.duration_since(*t).as_secs() >= CVD_WINDOW_SECS {
            window.pop_front();
        } else {
            break;
        }
    }
}

/// Flip all three connection health flags to false for this asset while the
/// single HL task is reconnecting (mirrors what the three Binance raptors do
/// individually on disconnect).
fn mark_offline(
    raptor_health_tx: &Arc<watch::Sender<HashMap<String, AssetRaptorHealth>>>,
    crypto_filter: &str,
) {
    raptor_health_tx.send_modify(|map| {
        let h = map.entry(crypto_filter.to_string()).or_default();
        h.price_connected = false;
        h.funding_connected = false;
        h.deriv_connected = false;
    });
}

/// Normalize a Hyperliquid HOURLY funding rate onto Binance's per-8h scale so
/// the `BASIS_*_FUNDING_THRESHOLD` viper gates (tuned on 8h `lastFundingRate`)
/// keep their meaning: multiply by 8.
fn normalize_funding_8h(hourly: Decimal) -> Decimal {
    hourly * dec!(8)
}

/// Rolling taker CVD ratio = buy_vol / sell_vol over the window. When the sell
/// window is empty (would divide by zero) keep the previous ratio, matching the
/// "no fresh data → hold last" semantics of Binance's `buySellRatio`.
fn cvd_ratio_or_prev(buy_sum: Decimal, sell_sum: Decimal, prev: Decimal) -> Decimal {
    if sell_sum > dec!(0) {
        buy_sum / sell_sum
    } else {
        prev
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn funding_normalized_hourly_to_8h() {
        // A +0.00125%/hr rate becomes +0.01%/8h — the scale the viper thresholds expect.
        assert_eq!(normalize_funding_8h(dec!(0.0000125)), dec!(0.0001));
        assert_eq!(normalize_funding_8h(dec!(-0.0000125)), dec!(-0.0001));
        assert_eq!(normalize_funding_8h(dec!(0)), dec!(0));
    }

    #[test]
    fn cvd_ratio_basic_division() {
        // 60 buy / 40 sell = 1.5 (buy-side aggression).
        assert_eq!(cvd_ratio_or_prev(dec!(60), dec!(40), dec!(1)), dec!(1.5));
    }

    #[test]
    fn cvd_ratio_zero_sell_keeps_previous() {
        // No sell volume in the window → hold the previous ratio, never divide by 0.
        assert_eq!(cvd_ratio_or_prev(dec!(25), dec!(0), dec!(1.2)), dec!(1.2));
        // Initial neutral seed (1.0) is preserved before any sell volume arrives.
        assert_eq!(cvd_ratio_or_prev(dec!(0), dec!(0), dec!(1)), dec!(1));
    }
}
