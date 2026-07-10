//! Hyperliquid historical-market-data provider — the DRADIS default `--source`.
//!
//! Wire code moved verbatim from the pre-refactor `fetch.rs::{fetch_candles_range,
//! fetch_funding_range}`: plain `reqwest` POSTs against the public Hyperliquid Info
//! API (`https://api.hyperliquid.xyz/info`) — the same wire shape
//! `helpers::time.rs::fetch_hyperliquid_close` already uses, so no
//! `hyperliquid_rust_sdk` and no `hyperliquid` cargo feature are required here (that
//! feature stays reserved for the live SDK raptor).
//!
//!   * `candleSnapshot` — OHLCV. The endpoint caps each response at ~5000 candles
//!     and anchors responses at the TAIL, retaining only the most recent ~5000
//!     candles — a requested window older than that is head-truncated at the
//!     source (unfetchable; the harness detects and warns about this). Every OHLCV
//!     field is a JSON STRING, parsed via `fetch::dec`. Pagination advances
//!     `startTime` past the last candle returned, with a forward-progress guard and
//!     a short-page (< 4000 rows) tail stop.
//!   * `fundingHistory` — hourly funding series, paginated the same way with a
//!     `max_t + 1` cursor and a short-page (< 400 rows) tail stop.
//!
//! This provider owns ONLY the wire protocol: it returns plain
//! `Vec<Candle>`/`Vec<FundingPoint>` and never touches SQLite. In particular, the
//! "never persist a still-forming candle" rule that used to live in this file's
//! pagination loop has moved to `BacktestCache::upsert_candles` — a
//! caching-correctness rule, not a wire-protocol one — so `fetch_candles` MAY
//! return the currently-forming final bar.

use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

use super::{FundingPoint, HistoricalSource};
use crate::backtest::fetch::{dec, interval_ms, Candle};

const INFO_URL: &str = "https://api.hyperliquid.xyz/info";
const PAGE_DELAY: Duration = Duration::from_millis(200);

pub struct HyperliquidSource {
    http: reqwest::Client,
}

impl HyperliquidSource {
    /// Hyperliquid's public `info` API is unauthenticated — it has no auth
    /// field at all, so this provider takes no credentials of any kind.
    pub fn new() -> Self {
        Self { http: reqwest::Client::new() }
    }
}

impl Default for HyperliquidSource {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HistoricalSource for HyperliquidSource {
    fn id(&self) -> &'static str {
        "hyperliquid"
    }

    fn resolve_symbol(&self, coin: &str) -> String {
        coin.to_uppercase()
    }

    fn funding_period_hours(&self) -> u32 {
        1
    }

    /// Paginated `candleSnapshot` fetch. Ascending by open-time `ts_ms`. MAY
    /// include the still-forming final bar — the cache strips it on insert.
    async fn fetch_candles(
        &self,
        coin: &str,
        interval: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<Candle>> {
        let step = interval_ms(interval)?;
        let mut cursor = start_ms;
        let mut out = Vec::new();
        while cursor < end_ms {
            let body = serde_json::json!({
                "type": "candleSnapshot",
                "req": { "coin": coin, "interval": interval, "startTime": cursor, "endTime": end_ms }
            });
            let json: serde_json::Value = self
                .http
                .post(INFO_URL)
                .json(&body)
                .send()
                .await
                .context("candleSnapshot POST failed")?
                .json()
                .await
                .context("candleSnapshot decode failed")?;
            let arr = json.as_array().cloned().unwrap_or_default();
            if arr.is_empty() {
                break;
            }
            let mut max_t = cursor;
            for c in &arr {
                let (Some(t), Some(o), Some(h), Some(l), Some(cl), Some(v)) = (
                    c.get("t").and_then(|x| x.as_i64()),
                    c.get("o").and_then(|x| x.as_str()),
                    c.get("h").and_then(|x| x.as_str()),
                    c.get("l").and_then(|x| x.as_str()),
                    c.get("c").and_then(|x| x.as_str()),
                    c.get("v").and_then(|x| x.as_str()),
                ) else {
                    continue;
                };
                if t >= end_ms {
                    continue;
                }
                max_t = max_t.max(t);
                out.push(Candle {
                    ts_ms: t,
                    open: dec(o)?,
                    high: dec(h)?,
                    low: dec(l)?,
                    close: dec(cl)?,
                    volume: dec(v)?,
                });
            }

            // Advance past the last candle we saw; guarantee forward progress.
            let next = max_t + step;
            if next <= cursor {
                break;
            }
            cursor = next;
            // Stop once a short page (< cap) tells us we reached the tail.
            if arr.len() < 4000 {
                break;
            }
            tokio::time::sleep(PAGE_DELAY).await;
        }
        Ok(out)
    }

    /// Paginated `fundingHistory` fetch. Ascending. Rates are the raw HOURLY HL
    /// rate — normalization onto the canonical per-8h scale happens in
    /// `synth::normalize_funding`, not here.
    async fn fetch_funding(
        &self,
        coin: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<FundingPoint>> {
        let mut cursor = start_ms;
        let mut out = Vec::new();
        while cursor < end_ms {
            let body = serde_json::json!({
                "type": "fundingHistory", "coin": coin, "startTime": cursor, "endTime": end_ms
            });
            let json: serde_json::Value = self
                .http
                .post(INFO_URL)
                .json(&body)
                .send()
                .await
                .context("fundingHistory POST failed")?
                .json()
                .await
                .context("fundingHistory decode failed")?;
            let arr = json.as_array().cloned().unwrap_or_default();
            if arr.is_empty() {
                break;
            }
            let mut max_t = cursor;
            for f in &arr {
                let (Some(t), Some(rate)) = (
                    f.get("time").and_then(|x| x.as_i64()),
                    f.get("fundingRate").and_then(|x| x.as_str()),
                ) else {
                    continue;
                };
                if t >= end_ms {
                    continue;
                }
                max_t = max_t.max(t);
                out.push(FundingPoint {
                    ts_ms: t,
                    rate: dec(rate)?,
                });
            }

            // Advance past the last observation we saw; guarantee forward progress.
            let next = max_t + 1;
            if next <= cursor {
                break;
            }
            cursor = next;
            // Stop once a short page (< cap) tells us we reached the tail.
            if arr.len() < 400 {
                break;
            }
            tokio::time::sleep(PAGE_DELAY).await;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use serde_json::Value;
    use std::str::FromStr;

    #[test]
    fn resolve_symbol_is_identity_uppercased() {
        let src = HyperliquidSource::new();
        assert_eq!(src.resolve_symbol("btc"), "BTC");
        assert_eq!(src.resolve_symbol("BTC"), "BTC");
        assert_eq!(src.resolve_symbol("Eth"), "ETH");
    }

    #[test]
    fn id_and_funding_period() {
        let src = HyperliquidSource::new();
        assert_eq!(src.id(), "hyperliquid");
        assert_eq!(src.funding_period_hours(), 1);
    }

    /// One raw `candleSnapshot` row: `t` is a JSON number, `o/h/l/c/v` are JSON
    /// STRINGS parsed via `fetch::dec`, never `as_f64`.
    #[test]
    fn candle_snapshot_row_ohlcv_strings_parse_as_decimal() {
        let row: Value = serde_json::from_str(
            r#"{"t":1625097600000,"o":"42055.50","h":"42200.00","l":"41950.10",
               "c":"42100.75","v":"1234.567","T":1625097659999,"s":"BTC","i":"1m"}"#,
        )
        .unwrap();
        assert_eq!(row.get("t").and_then(Value::as_i64).unwrap(), 1625097600000);
        let o = dec(row.get("o").and_then(Value::as_str).unwrap()).unwrap();
        assert_eq!(o, Decimal::from_str("42055.50").unwrap());
        let c = dec(row.get("c").and_then(Value::as_str).unwrap()).unwrap();
        assert_eq!(c, Decimal::from_str("42100.75").unwrap());
    }

    /// One raw `fundingHistory` object: `time` is a JSON number, `fundingRate` is a
    /// JSON string parsed via `fetch::dec`; native hourly cadence, normalized only
    /// in `synth::normalize_funding`.
    #[test]
    fn funding_history_object_rate_string_parses_as_decimal() {
        let obj: Value = serde_json::from_str(
            r#"{"coin":"BTC","fundingRate":"0.0000125","premium":"0.0001","time":1625097600000}"#,
        )
        .unwrap();
        assert_eq!(obj.get("time").and_then(Value::as_i64).unwrap(), 1625097600000);
        let rate = dec(obj.get("fundingRate").and_then(Value::as_str).unwrap()).unwrap();
        assert_eq!(rate, Decimal::from_str("0.0000125").unwrap());
    }
}
