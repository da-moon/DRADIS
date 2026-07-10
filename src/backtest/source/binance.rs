//! Binance FAPI historical-market-data provider.
//!
//! Reads candles and funding history from Binance USDⓈ-M **futures** (FAPI,
//! `fapi.binance.com`), never spot: funding only exists on futures, pulling both
//! series from the same instrument keeps them time-aligned, and perp prices embed
//! the basis a perp strategy actually experiences. FAPI is already this codebase's
//! canonical Binance host (`raptors/funding.rs`, `raptors/derivatives.rs`).
//!
//! Plain `reqwest` GETs, no SDK — same shape as the Hyperliquid provider. Each
//! endpoint owns its own pagination loop (Binance's page caps and cursor rules
//! differ from Hyperliquid's, hence no shared driver):
//!   * `GET /fapi/v1/klines` — `limit=1500` (the empirically verified ceiling; the
//!     SDK docstring's "max 1000" is stale — 1501 rows returns HTTP 400 `-1130`).
//!     Each row is a 12-element array; OHLCV (indices 1-5) are JSON STRINGS,
//!     parsed via `fetch::dec`, never `as_f64`.
//!   * `GET /fapi/v1/fundingRate` — `limit=1000`, with an ALWAYS-explicit
//!     `startTime`: verified live, omitting it silently caps the response at
//!     ~500 rows regardless of `limit`. Rates are cached in Binance's native
//!     per-8h cadence; normalization is `synth::normalize_funding`'s job.
//!
//! No retry logic in v1 (matches the Hyperliquid provider); the 200ms inter-page
//! delay keeps us far from 429/418 alongside the live funding/derivatives raptors
//! already polling this same host from this IP.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use tracing::info;

use super::{FundingPoint, HistoricalSource};
use crate::backtest::fetch::{dec, interval_ms, Candle};

const KLINES_URL: &str = "https://fapi.binance.com/fapi/v1/klines";
const FUNDING_URL: &str = "https://fapi.binance.com/fapi/v1/fundingRate";
const PAGE_DELAY: Duration = Duration::from_millis(200);

pub struct BinanceSource {
    http: reqwest::Client,
}

impl BinanceSource {
    /// FAPI market-data endpoints are public and IP-rate-limited — an API key
    /// changes nothing for them (verified live), so this provider takes no
    /// credentials of any kind.
    pub fn new() -> Self {
        Self { http: reqwest::Client::new() }
    }
}

impl Default for BinanceSource {
    fn default() -> Self {
        Self::new()
    }
}

impl BinanceSource {
    /// Shared GET-and-decode path for both endpoints — critically, it checks
    /// the HTTP status BEFORE attempting a JSON decode. Binance's error
    /// envelope is NOT uniform: most endpoints return `{"code":-1121,"msg":"..."}`,
    /// but `fundingRate` was observed live returning a WAF-style
    /// `{status,type,code,errorData}` shape on failure — so we never assume a
    /// `{code,msg}` shape, just surface the raw (truncated) body.
    async fn get_json(&self, url: &str, query: &[(&str, String)]) -> Result<Value> {
        let resp = self
            .http
            .get(url)
            .query(query)
            .send()
            .await
            .with_context(|| format!("binance request failed: {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let truncated: String = body.chars().take(300).collect();
            bail!("binance {status}: {truncated}");
        }
        resp.json::<Value>()
            .await
            .with_context(|| format!("binance response decode failed: {url}"))
    }
}

#[async_trait]
impl HistoricalSource for BinanceSource {
    fn id(&self) -> &'static str {
        "binance"
    }

    /// Map a DRADIS coin (e.g. "BTC") onto Binance FAPI's wire symbol:
    /// `{COIN}USDT`. Correct for BTC/ETH/SOL — verified live.
    ///
    /// **Known limitation:** Binance FUTURES rebases low-price meme coins into
    /// the symbol itself (`1000PEPEUSDT`, even `1000000MOGUSDT`), while spot
    /// lists them unprefixed and Hyperliquid uses `kPEPE` — three different
    /// naming schemes for the same coin. If the tradable universe grows past
    /// majors, this must become an explicit per-coin lookup table (cf.
    /// `raptors/source.rs`'s per-venue symbol tables). Chosen failure mode:
    /// naive concat + loud failure — an unmapped coin surfaces Binance's own
    /// `{"code":-1121,"msg":"Invalid symbol."}` as a hard error; it can never
    /// silently mis-map for the majors DRADIS actually trades.
    fn resolve_symbol(&self, coin: &str) -> String {
        format!("{}USDT", coin.to_uppercase())
    }

    /// Binance FAPI majors settle funding every 8h.
    ///
    /// **Known limitation:** this is hardcoded — Binance runs non-8h cadences
    /// on some symbols (queryable via `/fapi/v1/fundingInfo`, weight 0). Safe
    /// for BTC/ETH/SOL; revisit before expanding the universe.
    fn funding_period_hours(&self) -> u32 {
        8
    }

    async fn fetch_candles(
        &self,
        coin: &str,
        interval: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<Candle>> {
        let symbol = self.resolve_symbol(coin);
        let step = interval_ms(interval)?;
        let mut cursor = start_ms;
        let mut out = Vec::new();

        while cursor < end_ms {
            let params = [
                ("symbol", symbol.clone()),
                ("interval", interval.to_string()),
                ("startTime", cursor.to_string()),
                ("endTime", end_ms.to_string()),
                ("limit", "1500".to_string()),
            ];
            let json = self.get_json(KLINES_URL, &params).await?;
            let arr = json.as_array().cloned().unwrap_or_default();
            if arr.is_empty() {
                break;
            }
            let page_len = arr.len();
            // Binance returns klines ascending by open time, so the last row in the
            // page carries the highest openTime regardless of whether it fell inside
            // [start_ms, end_ms) — used for the cursor advance below, independent of
            // which rows we actually keep.
            let last_open_time = arr
                .last()
                .and_then(|row| row.as_array())
                .and_then(|cells| cells.first())
                .and_then(Value::as_i64)
                .unwrap_or(cursor);

            for row in &arr {
                let Some(cells) = row.as_array() else {
                    continue;
                };
                if cells.len() < 6 {
                    continue;
                }
                let Some(ts_ms) = cells[0].as_i64() else {
                    continue;
                };
                if ts_ms >= end_ms {
                    continue;
                }
                let (Some(o), Some(h), Some(l), Some(c), Some(v)) = (
                    cells[1].as_str(),
                    cells[2].as_str(),
                    cells[3].as_str(),
                    cells[4].as_str(),
                    cells[5].as_str(),
                ) else {
                    continue;
                };
                out.push(Candle {
                    ts_ms,
                    open: dec(o)?,
                    high: dec(h)?,
                    low: dec(l)?,
                    close: dec(c)?,
                    volume: dec(v)?,
                });
            }

            // Advance past the last candle Binance returned; guarantee forward
            // progress so a malformed/empty-ish page can never spin forever.
            let next = last_open_time + step;
            if next <= cursor {
                break;
            }
            cursor = next;
            if page_len < 1500 || cursor >= end_ms {
                break; // short page (tail reached) or window exhausted
            }
            tokio::time::sleep(PAGE_DELAY).await;
        }

        info!("✅ binance candles fetched: {} rows [{symbol} {interval}]", out.len());
        Ok(out)
    }

    async fn fetch_funding(
        &self,
        coin: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<FundingPoint>> {
        let symbol = self.resolve_symbol(coin);
        let mut cursor = start_ms;
        let mut out = Vec::new();

        while cursor < end_ms {
            let params = [
                ("symbol", symbol.clone()),
                // ALWAYS explicit — verified live: omitting startTime silently caps
                // the response at ~500 rows regardless of `limit`.
                ("startTime", cursor.to_string()),
                ("endTime", end_ms.to_string()),
                ("limit", "1000".to_string()),
            ];
            let json = self.get_json(FUNDING_URL, &params).await?;
            let arr = json.as_array().cloned().unwrap_or_default();
            if arr.is_empty() {
                break;
            }
            let page_len = arr.len();
            let last_funding_time = arr
                .last()
                .and_then(|o| o.get("fundingTime"))
                .and_then(Value::as_i64)
                .unwrap_or(cursor);

            for obj in &arr {
                let Some(ts_ms) = obj.get("fundingTime").and_then(Value::as_i64) else {
                    continue;
                };
                if ts_ms >= end_ms {
                    continue;
                }
                // `markPrice` may be an empty string pre-2020 and is unused here.
                let Some(rate) = obj.get("fundingRate").and_then(Value::as_str) else {
                    continue;
                };
                out.push(FundingPoint { ts_ms, rate: dec(rate)? });
            }

            let next = last_funding_time + 1;
            if next <= cursor {
                break;
            }
            cursor = next;
            if page_len < 1000 || cursor >= end_ms {
                break;
            }
            tokio::time::sleep(PAGE_DELAY).await;
        }

        info!("✅ binance funding fetched: {} rows [{symbol}]", out.len());
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    #[test]
    fn resolve_symbol_appends_usdt() {
        let src = BinanceSource::new();
        assert_eq!(src.resolve_symbol("btc"), "BTCUSDT");
        assert_eq!(src.resolve_symbol("BTC"), "BTCUSDT");
        assert_eq!(src.resolve_symbol("eth"), "ETHUSDT");
    }

    #[test]
    fn id_and_funding_period() {
        let src = BinanceSource::new();
        assert_eq!(src.id(), "binance");
        assert_eq!(src.funding_period_hours(), 8);
    }

    /// One raw `/fapi/v1/klines` row: 12-element array, OHLCV as JSON strings.
    #[test]
    fn kline_row_ohlcv_strings_parse_as_decimal() {
        let row: Value = serde_json::from_str(
            r#"[1625097600000,"42055.50","42200.00","41950.10","42100.75","1234.567",
               1625097659999,"51987654.32",1000,"600.111","25987654.32","0"]"#,
        )
        .unwrap();
        let cells = row.as_array().unwrap();
        assert_eq!(cells[0].as_i64().unwrap(), 1625097600000);
        assert_eq!(dec(cells[1].as_str().unwrap()).unwrap(), Decimal::from_str("42055.50").unwrap());
        assert_eq!(dec(cells[4].as_str().unwrap()).unwrap(), Decimal::from_str("42100.75").unwrap());
    }

    /// One raw `/fapi/v1/fundingRate` object; `markPrice` may be empty pre-2020
    /// and is intentionally ignored.
    #[test]
    fn funding_object_rate_string_parses_as_decimal() {
        let obj: Value = serde_json::from_str(
            r#"{"symbol":"BTCUSDT","fundingTime":1625097600000,"fundingRate":"0.00010000","markPrice":""}"#,
        )
        .unwrap();
        assert_eq!(obj.get("fundingTime").and_then(Value::as_i64).unwrap(), 1625097600000);
        let rate = dec(obj.get("fundingRate").and_then(Value::as_str).unwrap()).unwrap();
        assert_eq!(rate, Decimal::from_str("0.00010000").unwrap());
    }

    /// A non-`{code,msg}` (WAF-style) error body must not be assumed — the caller
    /// only needs it to surface as an opaque error, not to parse a specific shape.
    #[test]
    fn non_standard_error_envelope_is_opaque_not_parsed() {
        let body = r#"{"status":403,"type":"about:blank","code":0,"errorData":null}"#;
        let parsed: Result<Value, _> = serde_json::from_str(body);
        assert!(parsed.is_ok()); // valid JSON, just not the {code,msg} shape
        let v = parsed.unwrap();
        assert!(v.get("code").is_some()); // has "code" but as an int, not the -1xxx string envelope
        assert!(v.get("msg").is_none()); // no "msg" field at all — {code,msg} assumption would fail here
    }
}
