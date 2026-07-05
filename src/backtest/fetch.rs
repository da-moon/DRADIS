//! W2 — Historical data fetch + SQLite cache.
//!
//! Async fetchers against the public Hyperliquid Info API
//! (`https://api.hyperliquid.xyz/info`) using plain `reqwest` 0.12 POSTs — the same
//! wire shape `src/helpers/time.rs::fetch_hyperliquid_close` already uses, so no SDK
//! and no `hyperliquid` cargo feature are required.
//!
//!   * `candleSnapshot` — 1m OHLCV. The endpoint caps each response at ~5000 candles,
//!     so we paginate by advancing `startTime` past the last returned candle. Every
//!     OHLCV field is a JSON STRING → parsed with `Decimal::from_str`. NOTE: the API
//!     also RETAINS only the most recent ~5000 candles and anchors responses at the
//!     TAIL, so a requested window older than ~5000×interval is head-truncated at the
//!     source (unfetchable) — the harness detects this and warns; the still-forming
//!     current candle is never cached (its OHLCV is provisional until the bar closes).
//!   * `fundingHistory` — hourly funding series, paginated the same way.
//!
//! Results are cached in a SEPARATE SQLite file (default `backtest_cache.sqlite`,
//! `--cache <path>`), with its OWN pool/schema — the live trading DB is never touched.
//! Fetches fill gaps only: a range already densely cached is served from disk with no
//! network round-trip. Pages are rate-limited (~200ms) to stay polite.

use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use rust_decimal::Decimal;
use sqlx::{sqlite::SqlitePoolOptions, Row, SqlitePool};
use tracing::{debug, info};

/// One OHLCV bar. Timestamps are the candle OPEN time in unix milliseconds (UTC).
#[derive(Debug, Clone)]
pub struct Candle {
    pub ts_ms: i64,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
}

/// One funding observation: the HL HOURLY funding rate at `ts_ms` (UTC millis).
/// Normalization to Binance's per-8h scale (×8) happens in `synth`, not here — we
/// cache the raw rate exactly as the API returned it.
#[derive(Debug, Clone)]
pub struct FundingPoint {
    pub ts_ms: i64,
    pub rate: Decimal,
}

const INFO_URL: &str = "https://api.hyperliquid.xyz/info";
const PAGE_DELAY: Duration = Duration::from_millis(200);
/// Interval step in ms for the 1m default (used for coverage/gap checks).
fn interval_ms(interval: &str) -> i64 {
    match interval {
        "1m" => 60_000,
        "3m" => 180_000,
        "5m" => 300_000,
        "15m" => 900_000,
        "30m" => 1_800_000,
        "1h" => 3_600_000,
        "4h" => 14_400_000,
        "1d" => 86_400_000,
        _ => 60_000,
    }
}

/// Backtest-only SQLite cache. Distinct pool + schema from `helpers::db` — never
/// shares the live trading database.
pub struct BacktestCache {
    pool: SqlitePool,
}

impl BacktestCache {
    /// Open (creating if absent) the cache DB at `path` and ensure the schema.
    pub async fn open(path: &str) -> Result<Self> {
        let url = format!("sqlite://{}?mode=rwc", path);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .with_context(|| format!("opening backtest cache at {path}"))?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS candles (
                coin     TEXT NOT NULL,
                interval TEXT NOT NULL,
                ts       INTEGER NOT NULL,
                o        TEXT NOT NULL,
                h        TEXT NOT NULL,
                l        TEXT NOT NULL,
                c        TEXT NOT NULL,
                v        TEXT NOT NULL,
                PRIMARY KEY (coin, interval, ts)
            )",
        )
        .execute(&pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS funding (
                coin TEXT NOT NULL,
                ts   INTEGER NOT NULL,
                rate TEXT NOT NULL,
                PRIMARY KEY (coin, ts)
            )",
        )
        .execute(&pool)
        .await?;

        info!("📦 Backtest cache ready: {path}");
        Ok(Self { pool })
    }

    /// Handle to the underlying pool so the LLM-score cache can share this DB file.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    // ── Candles ───────────────────────────────────────────────────────────────

    /// Load candles for `[start_ms, end_ms)`, fetching from Hyperliquid only for the
    /// portion of the range not already densely cached. Returns them sorted ascending.
    pub async fn load_candles(
        &self,
        http: &reqwest::Client,
        coin: &str,
        interval: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<Candle>> {
        let step = interval_ms(interval);
        let expected = ((end_ms - start_ms) / step).max(1);

        let row = sqlx::query(
            "SELECT COUNT(*) AS n, MIN(ts) AS lo, MAX(ts) AS hi
             FROM candles WHERE coin=? AND interval=? AND ts>=? AND ts<?",
        )
        .bind(coin)
        .bind(interval)
        .bind(start_ms)
        .bind(end_ms)
        .fetch_one(&self.pool)
        .await?;
        let n: i64 = row.get("n");
        let lo: Option<i64> = row.try_get("lo").ok();
        let hi: Option<i64> = row.try_get("hi").ok();

        let covered = n > 0
            && lo.map(|l| l <= start_ms + 2 * step).unwrap_or(false)
            && hi.map(|h| h >= end_ms - 2 * step).unwrap_or(false)
            && n as f64 >= expected as f64 * 0.90;

        if covered {
            debug!("🗃️  candles [{coin} {interval}] served from cache ({n} rows)");
        } else {
            info!("🌐 fetching candles [{coin} {interval}] {start_ms}..{end_ms} (cache had {n})");
            self.fetch_candles_range(http, coin, interval, start_ms, end_ms).await?;
        }

        let rows = sqlx::query(
            "SELECT ts, o, h, l, c, v FROM candles
             WHERE coin=? AND interval=? AND ts>=? AND ts<? ORDER BY ts ASC",
        )
        .bind(coin)
        .bind(interval)
        .bind(start_ms)
        .bind(end_ms)
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(Candle {
                ts_ms: r.get::<i64, _>("ts"),
                open: dec(r.get::<String, _>("o"))?,
                high: dec(r.get::<String, _>("h"))?,
                low: dec(r.get::<String, _>("l"))?,
                close: dec(r.get::<String, _>("c"))?,
                volume: dec(r.get::<String, _>("v"))?,
            });
        }
        Ok(out)
    }

    /// Paginated `candleSnapshot` fetch, upserting each page (INSERT OR IGNORE).
    async fn fetch_candles_range(
        &self,
        http: &reqwest::Client,
        coin: &str,
        interval: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<()> {
        let step = interval_ms(interval);
        // Wall-clock now, to reject the currently-forming candle (see the `t + step`
        // check below): its provisional OHLCV keeps moving until the interval closes,
        // and `INSERT OR IGNORE` would cache those partial values PERMANENTLY, making
        // reruns non-reproducible against the final bar.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(i64::MAX);
        let mut cursor = start_ms;
        let mut total = 0usize;
        while cursor < end_ms {
            let body = serde_json::json!({
                "type": "candleSnapshot",
                "req": { "coin": coin, "interval": interval, "startTime": cursor, "endTime": end_ms }
            });
            let json: serde_json::Value = http
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
            let mut tx = self.pool.begin().await?;
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
                // Skip the still-forming candle for the current interval: it closes at
                // `t + step`, so if that is in the future its OHLCV is provisional and
                // must never be cached (would be frozen by INSERT OR IGNORE forever).
                if t + step > now_ms {
                    continue;
                }
                max_t = max_t.max(t);
                sqlx::query(
                    "INSERT OR IGNORE INTO candles (coin,interval,ts,o,h,l,c,v)
                     VALUES (?,?,?,?,?,?,?,?)",
                )
                .bind(coin)
                .bind(interval)
                .bind(t)
                .bind(o)
                .bind(h)
                .bind(l)
                .bind(cl)
                .bind(v)
                .execute(&mut *tx)
                .await?;
                total += 1;
            }
            tx.commit().await?;

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
        info!("✅ candles fetched/cached: {total} rows [{coin} {interval}]");
        Ok(())
    }

    // ── Funding ───────────────────────────────────────────────────────────────

    /// Load the funding series for `[start_ms, end_ms)`, fetching from Hyperliquid
    /// only when the cached range does not already cover it. Sorted ascending.
    pub async fn load_funding(
        &self,
        http: &reqwest::Client,
        coin: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<FundingPoint>> {
        let hour = 3_600_000i64;
        let row = sqlx::query(
            "SELECT COUNT(*) AS n, MIN(ts) AS lo, MAX(ts) AS hi
             FROM funding WHERE coin=? AND ts>=? AND ts<?",
        )
        .bind(coin)
        .bind(start_ms)
        .bind(end_ms)
        .fetch_one(&self.pool)
        .await?;
        let n: i64 = row.get("n");
        let lo: Option<i64> = row.try_get("lo").ok();
        let hi: Option<i64> = row.try_get("hi").ok();
        let covered = n > 0
            && lo.map(|l| l <= start_ms + 2 * hour).unwrap_or(false)
            && hi.map(|h| h >= end_ms - 2 * hour).unwrap_or(false);

        if covered {
            debug!("🗃️  funding [{coin}] served from cache ({n} rows)");
        } else {
            info!("🌐 fetching funding [{coin}] {start_ms}..{end_ms} (cache had {n})");
            self.fetch_funding_range(http, coin, start_ms, end_ms).await?;
        }

        let rows = sqlx::query(
            "SELECT ts, rate FROM funding WHERE coin=? AND ts>=? AND ts<? ORDER BY ts ASC",
        )
        .bind(coin)
        .bind(start_ms)
        .bind(end_ms)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(FundingPoint {
                ts_ms: r.get::<i64, _>("ts"),
                rate: dec(r.get::<String, _>("rate"))?,
            });
        }
        Ok(out)
    }

    async fn fetch_funding_range(
        &self,
        http: &reqwest::Client,
        coin: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<()> {
        let mut cursor = start_ms;
        let mut total = 0usize;
        while cursor < end_ms {
            let body = serde_json::json!({
                "type": "fundingHistory", "coin": coin, "startTime": cursor, "endTime": end_ms
            });
            let json: serde_json::Value = http
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
            let mut tx = self.pool.begin().await?;
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
                sqlx::query("INSERT OR IGNORE INTO funding (coin,ts,rate) VALUES (?,?,?)")
                    .bind(coin)
                    .bind(t)
                    .bind(rate)
                    .execute(&mut *tx)
                    .await?;
                total += 1;
            }
            tx.commit().await?;
            let next = max_t + 1;
            if next <= cursor {
                break;
            }
            cursor = next;
            if arr.len() < 400 {
                break;
            }
            tokio::time::sleep(PAGE_DELAY).await;
        }
        info!("✅ funding fetched/cached: {total} rows [{coin}]");
        Ok(())
    }
}

fn dec(s: String) -> Result<Decimal> {
    Decimal::from_str(s.trim()).with_context(|| format!("parsing decimal from '{s}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_ms_known_values() {
        assert_eq!(interval_ms("1m"), 60_000);
        assert_eq!(interval_ms("1h"), 3_600_000);
        assert_eq!(interval_ms("weird"), 60_000); // graceful default
    }

    #[test]
    fn dec_parses_string_ohlcv() {
        assert_eq!(dec("42055.5".to_string()).unwrap(), Decimal::from_str("42055.5").unwrap());
        assert!(dec("not-a-number".to_string()).is_err());
    }
}
