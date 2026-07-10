//! W2 — Historical data fetch + SQLite cache.
//!
//! The cache is provider-agnostic: it drives coverage checks and SQLite storage
//! against any `&dyn HistoricalSource` (`source::HistoricalSource`), which owns the
//! ENTIRE wire protocol (endpoint, request shape, its own pagination loop, auth
//! header) for one historical-market-data provider — Hyperliquid's public info API
//! by default, or Binance FAPI via `--source binance`. See `source/mod.rs` for the
//! trait and `source/{hyperliquid,binance}.rs` for the two implementations.
//!
//! Rows are keyed by `(source, coin, interval, ts)` / `(source, coin, ts)` so
//! different providers never collide in the same cache file. `BacktestCache` centralizes
//! two provider-agnostic caching-correctness rules in its insert path (`upsert_candles`/
//! `upsert_funding`), so no provider implementation can get them wrong:
//!   * a candle is never persisted while it is still forming (`ts + step > now_ms`) —
//!     its OHLCV is provisional until the bar closes, and `INSERT OR IGNORE` would
//!     freeze those partial values PERMANENTLY, making reruns non-reproducible;
//!   * rows at or past `end_ms` are dropped (a provider may over-fetch its last page).
//!
//! Funding rates are cached RAW, in the provider's own native cadence (Hyperliquid:
//! hourly; Binance FAPI: per-8h) — normalization onto the canonical per-8h scale
//! happens in `synth::normalize_funding`, not here; we cache exactly what the API
//! returned.
//!
//! Results are cached in a SEPARATE SQLite file (default `backtest_cache.sqlite`,
//! `--cache <path>`), with its OWN pool/schema — the live trading DB is never touched.
//! Fetches fill gaps only: a range already densely cached is served from disk with no
//! network round-trip.

use std::str::FromStr;

use anyhow::{Context, Result};
use rust_decimal::Decimal;
use sqlx::{sqlite::SqlitePoolOptions, Row, SqlitePool};
use tracing::{debug, info};

use super::source::HistoricalSource;

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

/// One funding observation: the funding rate at `ts_ms` (UTC millis), in the
/// PROVIDER'S NATIVE cadence — see the `source` column it is cached under
/// (`HistoricalSource::funding_period_hours`). Normalization to the canonical
/// per-8h scale happens in `synth`, not here — we cache the raw rate exactly as
/// the API returned it.
#[derive(Debug, Clone)]
pub struct FundingPoint {
    pub ts_ms: i64,
    pub rate: Decimal,
}

/// Interval step in ms (used for coverage/gap checks and still-forming-bar math).
/// Covers both Hyperliquid's and Binance FAPI's interval vocabularies.
///
/// Unknown intervals are a HARD ERROR, never a default: some strings we don't
/// enumerate are wire-valid (Binance FAPI happily serves `1M` monthly candles),
/// and silently assuming a 1-minute step there would both paginate wrongly and —
/// worse — defeat `upsert_candles`'s still-forming-bar guard (`ts + step > now`),
/// permanently freezing a provisional wide bar's OHLCV into the cache.
pub(crate) fn interval_ms(interval: &str) -> Result<i64> {
    Ok(match interval {
        "1m" => 60_000,
        "3m" => 180_000,
        "5m" => 300_000,
        "15m" => 900_000,
        "30m" => 1_800_000,
        "1h" => 3_600_000,
        "2h" => 7_200_000,
        "4h" => 14_400_000,
        "6h" => 21_600_000,
        "8h" => 28_800_000,
        "12h" => 43_200_000,
        "1d" => 86_400_000,
        "3d" => 259_200_000,
        "1w" => 604_800_000,
        other => anyhow::bail!(
            "unsupported --interval '{other}' (supported: 1m 3m 5m 15m 30m 1h 2h 4h 6h 8h 12h 1d 3d 1w)"
        ),
    })
}

/// Backtest-only SQLite cache. Distinct pool + schema from `helpers::db` — never
/// shares the live trading database.
pub struct BacktestCache {
    pool: SqlitePool,
}

impl BacktestCache {
    /// Open (creating if absent) the cache DB at `path`, migrate a legacy
    /// (pre-`source`-column) schema if present, and ensure the current schema.
    pub async fn open(path: &str) -> Result<Self> {
        let url = format!("sqlite://{}?mode=rwc", path);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .with_context(|| format!("opening backtest cache at {path}"))?;

        migrate_legacy_schema(&pool, "candles", &["coin", "interval", "ts", "o", "h", "l", "c", "v"]).await?;
        migrate_legacy_schema(&pool, "funding", &["coin", "ts", "rate"]).await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS candles (
                source   TEXT NOT NULL,
                coin     TEXT NOT NULL,
                interval TEXT NOT NULL,
                ts       INTEGER NOT NULL,
                o        TEXT NOT NULL,
                h        TEXT NOT NULL,
                l        TEXT NOT NULL,
                c        TEXT NOT NULL,
                v        TEXT NOT NULL,
                PRIMARY KEY (source, coin, interval, ts)
            )",
        )
        .execute(&pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS funding (
                source TEXT NOT NULL,
                coin   TEXT NOT NULL,
                ts     INTEGER NOT NULL,
                rate   TEXT NOT NULL,
                PRIMARY KEY (source, coin, ts)
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

    /// Load candles for `[start_ms, end_ms)`, fetching from `source` only for the
    /// portion of the range not already densely cached under this source's key.
    /// Returns them sorted ascending.
    pub async fn load_candles(
        &self,
        source: &dyn HistoricalSource,
        coin: &str,
        interval: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<Candle>> {
        let sid = source.id();
        let step = interval_ms(interval)?;
        let expected = ((end_ms - start_ms) / step).max(1);

        let row = sqlx::query(
            "SELECT COUNT(*) AS n, MIN(ts) AS lo, MAX(ts) AS hi
             FROM candles WHERE source=? AND coin=? AND interval=? AND ts>=? AND ts<?",
        )
        .bind(sid)
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
            debug!("🗃️  candles [{sid}:{coin} {interval}] served from cache ({n} rows)");
        } else {
            info!("🌐 fetching candles [{sid}:{coin} {interval}] {start_ms}..{end_ms} (cache had {n})");
            let fetched = source.fetch_candles(coin, interval, start_ms, end_ms).await?;
            self.upsert_candles(sid, coin, interval, end_ms, step, fetched).await?;
        }

        let rows = sqlx::query(
            "SELECT ts, o, h, l, c, v FROM candles
             WHERE source=? AND coin=? AND interval=? AND ts>=? AND ts<? ORDER BY ts ASC",
        )
        .bind(sid)
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
                open: dec(&r.get::<String, _>("o"))?,
                high: dec(&r.get::<String, _>("h"))?,
                low: dec(&r.get::<String, _>("l"))?,
                close: dec(&r.get::<String, _>("c"))?,
                volume: dec(&r.get::<String, _>("v"))?,
            });
        }
        Ok(out)
    }

    /// Upsert a batch of already-fetched candles (INSERT OR IGNORE), centralizing
    /// the two provider-agnostic caching-correctness guards: never persist a
    /// still-forming bar, and never persist past `end_ms`. This is the ONE place
    /// either rule is enforced, so no provider can drift from the other.
    async fn upsert_candles(
        &self,
        sid: &str,
        coin: &str,
        interval: &str,
        end_ms: i64,
        step: i64,
        candles: Vec<Candle>,
    ) -> Result<()> {
        // Wall-clock now, to reject the currently-forming candle (see the `ts_ms + step`
        // check below): its provisional OHLCV keeps moving until the interval closes,
        // and `INSERT OR IGNORE` would cache those partial values PERMANENTLY, making
        // reruns non-reproducible against the final bar.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(i64::MAX);
        let mut tx = self.pool.begin().await?;
        let mut total = 0usize;
        for c in candles {
            if c.ts_ms >= end_ms {
                continue;
            }
            // Still-forming bar — provider-agnostic guard (see module doc).
            if c.ts_ms + step > now_ms {
                continue;
            }
            sqlx::query(
                "INSERT OR IGNORE INTO candles (source,coin,interval,ts,o,h,l,c,v)
                 VALUES (?,?,?,?,?,?,?,?,?)",
            )
            .bind(sid)
            .bind(coin)
            .bind(interval)
            .bind(c.ts_ms)
            .bind(c.open.to_string())
            .bind(c.high.to_string())
            .bind(c.low.to_string())
            .bind(c.close.to_string())
            .bind(c.volume.to_string())
            .execute(&mut *tx)
            .await?;
            total += 1;
        }
        tx.commit().await?;
        info!("✅ candles fetched/cached: {total} rows [{sid}:{coin} {interval}]");
        Ok(())
    }

    // ── Funding ───────────────────────────────────────────────────────────────

    /// Load the funding series for `[start_ms, end_ms)`, fetching from `source` only
    /// when the cached range does not already cover it (in this source's native
    /// cadence). Sorted ascending, rates in the provider's native cadence.
    pub async fn load_funding(
        &self,
        source: &dyn HistoricalSource,
        coin: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<FundingPoint>> {
        let sid = source.id();
        // Coverage slack in the PROVIDER'S cadence: an hourly series (HL) can
        // legitimately end up to ~1h before `end_ms`, but an 8h-cadence series
        // (Binance) can legitimately end up to ~8h before it — hardcoding the HL
        // slack here would make Binance's funding look permanently under-covered
        // and refetch every run.
        let cadence_ms = (source.funding_period_hours() as i64).max(1) * 3_600_000;
        let row = sqlx::query(
            "SELECT COUNT(*) AS n, MIN(ts) AS lo, MAX(ts) AS hi
             FROM funding WHERE source=? AND coin=? AND ts>=? AND ts<?",
        )
        .bind(sid)
        .bind(coin)
        .bind(start_ms)
        .bind(end_ms)
        .fetch_one(&self.pool)
        .await?;
        let n: i64 = row.get("n");
        let lo: Option<i64> = row.try_get("lo").ok();
        let hi: Option<i64> = row.try_get("hi").ok();
        let covered = n > 0
            && lo.map(|l| l <= start_ms + 2 * cadence_ms).unwrap_or(false)
            && hi.map(|h| h >= end_ms - 2 * cadence_ms).unwrap_or(false);

        if covered {
            debug!("🗃️  funding [{sid}:{coin}] served from cache ({n} rows)");
        } else {
            info!("🌐 fetching funding [{sid}:{coin}] {start_ms}..{end_ms} (cache had {n})");
            let fetched = source.fetch_funding(coin, start_ms, end_ms).await?;
            self.upsert_funding(sid, coin, end_ms, fetched).await?;
        }

        let rows = sqlx::query(
            "SELECT ts, rate FROM funding WHERE source=? AND coin=? AND ts>=? AND ts<? ORDER BY ts ASC",
        )
        .bind(sid)
        .bind(coin)
        .bind(start_ms)
        .bind(end_ms)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(FundingPoint {
                ts_ms: r.get::<i64, _>("ts"),
                rate: dec(&r.get::<String, _>("rate"))?,
            });
        }
        Ok(out)
    }

    /// Upsert a batch of already-fetched funding points (INSERT OR IGNORE). No
    /// still-forming-bar guard applies to funding (a discrete event, not an
    /// interval bar) — only the `ts < end_ms` bound.
    async fn upsert_funding(
        &self,
        sid: &str,
        coin: &str,
        end_ms: i64,
        points: Vec<FundingPoint>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let mut total = 0usize;
        for f in points {
            if f.ts_ms >= end_ms {
                continue;
            }
            sqlx::query("INSERT OR IGNORE INTO funding (source,coin,ts,rate) VALUES (?,?,?,?)")
                .bind(sid)
                .bind(coin)
                .bind(f.ts_ms)
                .bind(f.rate.to_string())
                .execute(&mut *tx)
                .await?;
            total += 1;
        }
        tx.commit().await?;
        info!("✅ funding fetched/cached: {total} rows [{sid}:{coin}]");
        Ok(())
    }
}

/// Data-preserving migration for one table from the pre-refactor (no `source`
/// column) schema to the source-keyed schema. A no-op if `table` doesn't exist yet
/// (fresh cache file) or already has a `source` column (already migrated) — so
/// calling this unconditionally on every `open()` is idempotent. MUST run BEFORE
/// the `CREATE TABLE IF NOT EXISTS` statements, since the migration itself creates
/// the new table.
///
/// Every pre-refactor row was fetched from Hyperliquid (it was the only source),
/// so migrated rows are tagged `source='hyperliquid'` — correct by construction.
/// This is a data-preserving `RENAME`+`INSERT`, never a `DROP`+recreate: Hyperliquid's
/// API retains only the most recent ~5000 candles, so dropping a user's existing
/// cache would PERMANENTLY destroy any history older than that, not just cause a
/// one-time refetch.
async fn migrate_legacy_schema(pool: &SqlitePool, table: &str, legacy_cols: &[&str]) -> Result<()> {
    let exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
    )
    .bind(table)
    .fetch_one(pool)
    .await?;
    if exists == 0 {
        return Ok(()); // fresh cache file — nothing to migrate
    }

    let has_source: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info(?) WHERE name='source'",
    )
    .bind(table)
    .fetch_one(pool)
    .await?;
    if has_source >= 1 {
        return Ok(()); // already migrated
    }

    let legacy_table = format!("{table}_legacy");
    let cols = legacy_cols.join(", ");
    let mut tx = pool.begin().await?;
    sqlx::query(&format!("ALTER TABLE {table} RENAME TO {legacy_table}"))
        .execute(&mut *tx)
        .await?;
    let create_sql = match table {
        "candles" => {
            "CREATE TABLE candles (
                source   TEXT NOT NULL,
                coin     TEXT NOT NULL,
                interval TEXT NOT NULL,
                ts       INTEGER NOT NULL,
                o        TEXT NOT NULL,
                h        TEXT NOT NULL,
                l        TEXT NOT NULL,
                c        TEXT NOT NULL,
                v        TEXT NOT NULL,
                PRIMARY KEY (source, coin, interval, ts)
            )"
        }
        "funding" => {
            "CREATE TABLE funding (
                source TEXT NOT NULL,
                coin   TEXT NOT NULL,
                ts     INTEGER NOT NULL,
                rate   TEXT NOT NULL,
                PRIMARY KEY (source, coin, ts)
            )"
        }
        _ => unreachable!("migrate_legacy_schema only called for candles/funding"),
    };
    sqlx::query(create_sql).execute(&mut *tx).await?;
    sqlx::query(&format!(
        "INSERT INTO {table} (source, {cols}) SELECT 'hyperliquid', {cols} FROM {legacy_table}"
    ))
    .execute(&mut *tx)
    .await?;
    sqlx::query(&format!("DROP TABLE {legacy_table}"))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    info!("📦 migrated legacy backtest cache → source-keyed schema ({table})");
    Ok(())
}

pub(crate) fn dec(s: &str) -> Result<Decimal> {
    Decimal::from_str(s.trim()).with_context(|| format!("parsing decimal from '{s}'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    #[test]
    fn interval_ms_known_values() {
        assert_eq!(interval_ms("1m").unwrap(), 60_000);
        assert_eq!(interval_ms("1h").unwrap(), 3_600_000);
        assert_eq!(interval_ms("8h").unwrap(), 28_800_000);
    }

    /// Unknown intervals must FAIL, never default: `1M` is wire-valid on Binance
    /// FAPI, and a silent 60s step would defeat the still-forming-bar guard and
    /// permanently cache a provisional monthly bar.
    #[test]
    fn interval_ms_rejects_unknown_intervals() {
        assert!(interval_ms("weird").is_err());
        assert!(interval_ms("1M").is_err());
        assert!(interval_ms("").is_err());
    }

    #[test]
    fn dec_parses_string_ohlcv() {
        assert_eq!(dec("42055.5").unwrap(), Decimal::from_str("42055.5").unwrap());
        assert!(dec("not-a-number").is_err());
    }

    // ── Async SQLite tests ───────────────────────────────────────────────────
    //
    // Each test opens its own uniquely-named SQLite file under the OS temp dir
    // (no shared fixture, no `tempfile` crate dependency needed) and removes it
    // on the way out. A dummy `HistoricalSource` stub panics if `fetch_candles`/
    // `fetch_funding` are ever actually invoked, so a test that expects to be
    // served entirely from cache also proves no network call was attempted.

    /// A `HistoricalSource` stub whose fetch methods panic — used wherever a test
    /// pre-seeds the cache such that `load_*` must be served without fetching.
    struct PanicSource {
        sid: &'static str,
        funding_hours: u32,
    }

    #[async_trait]
    impl HistoricalSource for PanicSource {
        fn id(&self) -> &'static str {
            self.sid
        }
        fn resolve_symbol(&self, coin: &str) -> String {
            coin.to_string()
        }
        fn funding_period_hours(&self) -> u32 {
            self.funding_hours
        }
        async fn fetch_candles(
            &self,
            _coin: &str,
            _interval: &str,
            _start_ms: i64,
            _end_ms: i64,
        ) -> Result<Vec<Candle>> {
            panic!("fetch_candles must not be called when the cache is already covered");
        }
        async fn fetch_funding(
            &self,
            _coin: &str,
            _start_ms: i64,
            _end_ms: i64,
        ) -> Result<Vec<FundingPoint>> {
            panic!("fetch_funding must not be called when the cache is already covered");
        }
    }

    /// A unique, unshared SQLite file path under the OS temp dir for one test.
    fn temp_db_path(tag: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!("dradis_backtest_fetch_test_{tag}_{nanos}.sqlite"))
            .to_string_lossy()
            .to_string()
    }

    /// Best-effort cleanup of a temp cache file and its SQLite sidecar files.
    fn cleanup_db(path: &str) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{path}-journal"));
        let _ = std::fs::remove_file(format!("{path}-wal"));
        let _ = std::fs::remove_file(format!("{path}-shm"));
    }

    fn test_candle(ts_ms: i64, close: &str) -> Candle {
        Candle {
            ts_ms,
            open: dec(close).unwrap(),
            high: dec(close).unwrap(),
            low: dec(close).unwrap(),
            close: dec(close).unwrap(),
            volume: Decimal::from_str("1").unwrap(),
        }
    }

    /// Cross-contamination regression: identical `(coin, interval, ts)` rows under
    /// two different `source` values must both survive, and each source's
    /// `load_candles` must return only its own row — proving the composite
    /// `(source, coin, interval, ts)` primary key actually isolates them.
    #[tokio::test]
    async fn candles_do_not_cross_contaminate_across_sources() {
        let path = temp_db_path("cross_contam");
        let cache = BacktestCache::open(&path).await.unwrap();

        let interval = "1m";
        let step = interval_ms(interval).unwrap();
        let ts = 1_700_000_000_000i64;
        // One interval-step-wide window so a single cached row already satisfies
        // the coverage check (expected == 1) and load_candles never calls fetch.
        let start_ms = ts;
        let end_ms = ts + step;

        cache
            .upsert_candles("hyperliquid", "BTC", interval, end_ms, step, vec![test_candle(ts, "100")])
            .await
            .unwrap();
        cache
            .upsert_candles("binance", "BTC", interval, end_ms, step, vec![test_candle(ts, "200")])
            .await
            .unwrap();

        let hl = PanicSource { sid: "hyperliquid", funding_hours: 1 };
        let bn = PanicSource { sid: "binance", funding_hours: 8 };

        let hl_rows = cache.load_candles(&hl, "BTC", interval, start_ms, end_ms).await.unwrap();
        let bn_rows = cache.load_candles(&bn, "BTC", interval, start_ms, end_ms).await.unwrap();

        assert_eq!(hl_rows.len(), 1);
        assert_eq!(hl_rows[0].close, Decimal::from_str("100").unwrap());
        assert_eq!(bn_rows.len(), 1);
        assert_eq!(bn_rows[0].close, Decimal::from_str("200").unwrap());

        cleanup_db(&path);
    }

    /// Legacy (pre-`source`-column) cache files must migrate their rows forward
    /// tagged `source='hyperliquid'` (every pre-refactor row WAS fetched from
    /// Hyperliquid), and a second `open()` on the already-migrated file must be a
    /// no-op (idempotent — no duplication, no error).
    #[tokio::test]
    async fn legacy_schema_migrates_to_source_keyed_and_reopen_is_idempotent() {
        let path = temp_db_path("legacy_migration");

        // Build a legacy-schema DB by hand: no `source` column, exactly the
        // pre-refactor primary key (coin, interval, ts).
        {
            let url = format!("sqlite://{}?mode=rwc", path);
            let pool = SqlitePoolOptions::new().max_connections(1).connect(&url).await.unwrap();
            sqlx::query(
                "CREATE TABLE candles (
                    coin TEXT NOT NULL, interval TEXT NOT NULL, ts INTEGER NOT NULL,
                    o TEXT NOT NULL, h TEXT NOT NULL, l TEXT NOT NULL, c TEXT NOT NULL, v TEXT NOT NULL,
                    PRIMARY KEY (coin, interval, ts)
                )",
            )
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO candles (coin, interval, ts, o, h, l, c, v)
                 VALUES ('BTC','1m',1700000000000,'100','100','100','100','1')",
            )
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "CREATE TABLE funding (
                    coin TEXT NOT NULL, ts INTEGER NOT NULL, rate TEXT NOT NULL,
                    PRIMARY KEY (coin, ts)
                )",
            )
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query("INSERT INTO funding (coin, ts, rate) VALUES ('BTC',1700000000000,'0.0000125')")
                .execute(&pool)
                .await
                .unwrap();
            pool.close().await;
        }

        // First open() runs the migration.
        let cache = BacktestCache::open(&path).await.unwrap();
        let candle_row = sqlx::query("SELECT source, coin FROM candles WHERE coin='BTC'")
            .fetch_one(cache.pool())
            .await
            .unwrap();
        let source: String = candle_row.get("source");
        assert_eq!(source, "hyperliquid");
        let funding_row = sqlx::query("SELECT source FROM funding WHERE coin='BTC'")
            .fetch_one(cache.pool())
            .await
            .unwrap();
        let funding_source: String = funding_row.get("source");
        assert_eq!(funding_source, "hyperliquid");
        drop(cache);

        // Second open() must be idempotent: no re-migration, no row duplication.
        let cache2 = BacktestCache::open(&path).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM candles WHERE coin='BTC'")
            .fetch_one(cache2.pool())
            .await
            .unwrap();
        assert_eq!(n, 1);
        let nf: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM funding WHERE coin='BTC'")
            .fetch_one(cache2.pool())
            .await
            .unwrap();
        assert_eq!(nf, 1);

        cleanup_db(&path);
    }

    /// `upsert_candles` must skip a candle whose bar has not yet closed
    /// (`ts_ms + step > now_ms`) — persisting it would freeze a provisional OHLCV
    /// value permanently under `INSERT OR IGNORE`, poisoning reruns.
    #[tokio::test]
    async fn upsert_candles_skips_still_forming_bar() {
        let path = temp_db_path("still_forming");
        let cache = BacktestCache::open(&path).await.unwrap();

        let interval = "1m";
        let step = interval_ms(interval).unwrap();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        // Closed bar: opened well over a step ago.
        let closed = test_candle(now_ms - 10 * step, "111");
        // Still forming: opened less than one step ago, so ts_ms + step > now_ms.
        let forming = test_candle(now_ms - step / 2, "222");

        cache
            .upsert_candles(
                "hyperliquid",
                "BTC",
                interval,
                now_ms + 10 * step,
                step,
                vec![closed, forming],
            )
            .await
            .unwrap();

        let rows = sqlx::query("SELECT ts, c FROM candles WHERE source='hyperliquid' AND coin='BTC'")
            .fetch_all(cache.pool())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "only the closed bar should have been persisted");
        let c: String = rows[0].get("c");
        assert_eq!(c, "111");

        cleanup_db(&path);
    }
}
