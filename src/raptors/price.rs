/// Price Raptor — Binance Spot WebSocket price feed.
///
/// Connects to the Binance `<symbol>@ticker` stream for the configured crypto pair
/// and broadcasts the following signals via `watch` channels:
///
/// │ Channel        │ Type                           │ Description                        │
/// │────────────────│────────────────────────────────│────────────────────────────────────│
/// │ oracle_tx      │ Decimal                        │ Current spot price                 │
/// │ velocity_tx    │ (Decimal, Decimal, Decimal)    │ (5s velocity, 1s velocity, accel)  │
/// │ drift_tx       │ (Decimal, Decimal)             │ (60-min drift, 10-min drift)       │
///
/// Reconnects automatically on:
///   • Disconnect or WS error
///   • 30s with no message at all (dead TCP / half-open socket)
///   • 60s with no *price tick* — catches "zombie" connections where Binance
///     keepalive pings reset the 30s timer but ticker text has silently stopped
///
/// Consumers should treat a `dec!(0)` oracle price as "not yet connected".
///
/// The velocity / acceleration / drift math lives in `kinematics::PriceKinematics`
/// so the Hyperliquid raptor derives byte-identical signals from its trade feed.
use std::str::FromStr;
use std::collections::HashMap;

use futures::StreamExt as _;
use rust_decimal::Decimal;
use tokio::sync::watch;
use tokio::time::{Duration, Instant, timeout as tokio_timeout};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{info, warn};
use std::sync::Arc;

use crate::config;
use crate::api::server::AssetRaptorHealth;
use crate::helpers::volatility::{normalized_hist_vol, range_pct};
use crate::raptors::kinematics::PriceKinematics;
use crate::raptors::source;

pub async fn run_price_raptor(
    crypto_filter: String,
    oracle_tx: watch::Sender<Decimal>,
    velocity_tx: watch::Sender<(Decimal, Decimal, Decimal)>,
    // Sends (drift_60m, drift_10m) — both raw USD Decimal values.
    // drift_10m fills the 5s–60m temporal gap for GBoost feature [18].
    drift_tx: watch::Sender<(Decimal, Decimal)>,
    raptor_health_tx: Arc<watch::Sender<HashMap<String, AssetRaptorHealth>>>,
) {
    let binance_pair = source::binance_ws_pair(&crypto_filter);
    let url_str = format!("wss://stream.binance.com:9443/ws/{}@ticker", binance_pair);
    // Rolling velocity/accel/drift accumulator — shared math with the HL raptor.
    let mut kin = PriceKinematics::new();
    // Throttle for the periodic realized-volatility telemetry log.
    // Seeded in the past so the first eligible tick logs immediately.
    let mut last_vol_log = Instant::now()
        .checked_sub(Duration::from_secs(3600))
        .unwrap_or_else(Instant::now);

    loop {
        // Bounded connect: an unbounded `connect_async().await` can hang forever on a
        // half-open TCP path or geo-block, silently wedging the task with no reconnect
        // and no log (observed 2026-07-07 — oracle price frozen for ~10h). Cap it.
        let conn = tokio_timeout(Duration::from_secs(20), connect_async(&url_str)).await;
        if let Ok(Ok((mut ws_stream, _))) = conn {
            info!(" Price Raptor connected to Binance for {}", binance_pair.to_uppercase());
            // Mark price raptor as healthy for this asset.
            raptor_health_tx.send_modify(|map| {
                map.entry(crypto_filter.clone()).or_default().price_connected = true;
            });
            // last_price_tick tracks when we last received an actual ticker text
            // message with a valid price.  Binance sends periodic WS ping frames
            // that reset the 30s tokio_timeout below but carry no price data.  A
            // "zombie" connection — alive at the TCP level but delivering no ticker
            // updates — would otherwise be invisible forever.  If 60s elapse with
            // no real price tick we force a reconnect regardless of ping activity.
            let mut last_price_tick = Instant::now();

            'ws: loop {
                // Staleness guard: independent of WS keepalive pings.
                if last_price_tick.elapsed() >= Duration::from_secs(60) {
                    warn!("⚠️ Price Raptor: no price tick in 60s (zombie WS — pings alive but ticker silent) — reconnecting");
                    break 'ws;
                }

                match tokio_timeout(Duration::from_secs(30), ws_stream.next()).await {
                    Ok(Some(Ok(msg))) => {
                        if let Message::Text(text) = msg {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                                if let Some(price_str) = v.get("c").and_then(|p| p.as_str()) {
                                    if let Ok(price) = Decimal::from_str(price_str) {
                                        let now = Instant::now();
                                        last_price_tick = now; // reset staleness clock
                                        let _ = oracle_tx.send(price);

                                        // Derive velocity / acceleration / drift.
                                        let sig = kin.on_price(now, price);
                                        let _ = velocity_tx.send((sig.velocity_5s, sig.velocity_1s, sig.acceleration));
                                        let _ = drift_tx.send((sig.drift_60m, sig.drift_10m));

                                        // Mirror the latest signal snapshot into the shared
                                        // raptor-health map so GET /api/telemetry can graph it.
                                        raptor_health_tx.send_modify(|map| {
                                            let h = map.entry(crypto_filter.clone()).or_default();
                                            h.oracle_price = price;
                                            h.velocity_5s  = sig.velocity_5s;
                                            h.velocity_1s  = sig.velocity_1s;
                                            h.acceleration = sig.acceleration;
                                            h.drift_60m    = sig.drift_60m;
                                            h.drift_10m    = sig.drift_10m;
                                        });

                                        // Periodic realized-volatility telemetry (~every 120s).
                                        // Shared oracle-vol math so any viper can calibrate its
                                        // own choppiness gates against a common 60m measure.
                                        if now.duration_since(last_vol_log).as_secs()
                                            >= config::GBOOST_PRED_LOG_INTERVAL_SECS
                                        {
                                            last_vol_log = now;
                                            let prices = kin.prices_60m_f64();
                                            if prices.len() >= 5 {
                                                info!(
                                                    " [{}] 60m realized-vol: hist_vol={:.4} (norm 0-1) | range={:.3}% | samples={}",
                                                    crypto_filter.to_uppercase(),
                                                    normalized_hist_vol(&prices),
                                                    range_pct(&prices),
                                                    prices.len(),
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // Stream closed cleanly or returned an error — reconnect.
                    Ok(None) | Ok(Some(Err(_))) => break 'ws,
                    // 30s elapsed with no message — silent stall; force reconnect.
                    Err(_) => {
                        warn!("⚠️ Price Raptor: no tick in 30s — reconnecting");
                        break 'ws;
                    }
                }
            }
        }
        warn!("⚠️ Price Raptor disconnected. Reconnecting in 5s...");
        // Mark price raptor as offline while reconnecting.
        raptor_health_tx.send_modify(|map| {
            map.entry(crypto_filter.clone()).or_default().price_connected = false;
        });
        kin.reset_velocity();
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
