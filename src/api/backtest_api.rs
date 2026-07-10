//! Feature-gated (`--features backtest`) Control Tower backtest API.
//!
//! An in-memory run registry plus three endpoints:
//!   POST /api/backtest/run        — spawn ONE run (409 while one is in progress)
//!   GET  /api/backtest/runs       — list runs, newest first (lightweight summaries)
//!   GET  /api/backtest/runs/{id}  — full report + equity + trades + llm scores
//!
//! rs-backtester's process-global CONFIG makes concurrent runs unsafe (documented in
//! BACKTEST-RECON.md), so the registry admits exactly ONE running job at a time.
//!
//! The whole module — and every field/route it adds to the server — is compiled ONLY
//! under `--features backtest`, so the default build is byte-identical.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use rust_decimal::prelude::FromStr;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::server::ApiState;
use crate::backtest::entry::{parse_time, run_and_collect, EquityPoint, TradeRecord};
use crate::backtest::harness::BacktestConfig;
use crate::backtest::source::SourceKind;

/// Default backtest cache — the CLI default; a SEPARATE sqlite file from the live DB.
const DEFAULT_CACHE_PATH: &str = "backtest_cache.sqlite";

/// Cap on retained registry entries. The registry lives in the LIVE process's
/// `ApiState` for its whole lifetime and each completed run holds a full
/// report + equity curve + trades payload, so without a bound repeated runs are a
/// monotonic RSS leak. We keep the most recent `MAX_RUNS`, evicting the oldest
/// FINISHED entries (never the in-flight run).
const MAX_RUNS: usize = 50;

// ── Registry types ─────────────────────────────────────────────────────────────

/// Lifecycle of one registry entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Running,
    Done,
    Failed,
}

/// Echo of the resolved run parameters (string-encoded Decimals; no key material).
#[derive(Debug, Clone, Serialize)]
pub struct RunParamsEcho {
    pub coin: String,
    pub interval: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub spread: String,
    pub depth: String,
    pub commission: String,
    pub starting: String,
    pub strategies: Option<Vec<String>>,
    pub llm_score: bool,
    pub source: String,
}

/// One registry entry — the full record served by `GET /api/backtest/runs/{id}`.
#[derive(Debug, Clone, Serialize)]
pub struct BacktestRun {
    pub id: String,
    pub params: RunParamsEcho,
    pub status: RunStatus,
    pub error: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
    /// Report JSON (identical structure to report.json) — populated on completion.
    pub report: Option<serde_json::Value>,
    pub equity: Option<Vec<EquityPoint>>,
    pub trades: Option<Vec<TradeRecord>>,
}

/// Lightweight list view — omits the heavy report/equity/trades payload.
#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub id: String,
    pub params: RunParamsEcho,
    pub status: RunStatus,
    pub error: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
}

impl RunSummary {
    fn from_run(r: &BacktestRun) -> Self {
        Self {
            id: r.id.clone(),
            params: r.params.clone(),
            status: r.status,
            error: r.error.clone(),
            started_at: r.started_at.clone(),
            finished_at: r.finished_at.clone(),
        }
    }
}

/// Shared in-memory registry — an `Arc<Mutex<..>>` field in `ApiState`.
#[derive(Clone, Default)]
pub struct BacktestRegistry {
    inner: Arc<Mutex<Vec<BacktestRun>>>,
}

impl BacktestRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<BacktestRun>> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// True while any run is still `Running`. Retained for the registry unit tests;
    /// the handler admits runs via the atomic [`try_begin`](Self::try_begin) instead.
    #[cfg(test)]
    fn is_busy(&self) -> bool {
        self.lock().iter().any(|r| r.status == RunStatus::Running)
    }

    /// Unconditional push (test helper — seeds multiple entries without the one-at-a-time
    /// admission that [`try_begin`](Self::try_begin) enforces).
    #[cfg(test)]
    fn insert(&self, run: BacktestRun) {
        let mut g = self.lock();
        g.push(run);
        Self::prune(&mut g);
    }

    /// Atomically admit ONE run: under a single `MutexGuard`, reject if a run is
    /// already `Running`, otherwise push the new entry. This closes the check-then-act
    /// race in the old `is_busy()` + `insert()` split (two independent lock scopes let
    /// concurrent POSTs both pass the guard and spawn overlapping runs that corrupt
    /// rs-backtester's process-global CONFIG). Returns `false` when a run is in progress.
    #[must_use]
    fn try_begin(&self, run: BacktestRun) -> bool {
        let mut g = self.lock();
        if g.iter().any(|r| r.status == RunStatus::Running) {
            return false;
        }
        g.push(run);
        Self::prune(&mut g);
        true
    }

    /// Evict the oldest FINISHED entries until at most `MAX_RUNS` remain. A `Running`
    /// entry is never evicted (skipped), so the in-flight run's payload is always kept.
    fn prune(g: &mut Vec<BacktestRun>) {
        while g.len() > MAX_RUNS {
            match g.iter().position(|r| r.status != RunStatus::Running) {
                Some(pos) => {
                    g.remove(pos);
                }
                None => break,
            }
        }
    }

    /// Transition a run to `Done` and attach its collected payload.
    fn complete(
        &self,
        id: &str,
        report: serde_json::Value,
        equity: Vec<EquityPoint>,
        trades: Vec<TradeRecord>,
    ) {
        let mut g = self.lock();
        if let Some(r) = g.iter_mut().find(|r| r.id == id) {
            r.status = RunStatus::Done;
            r.finished_at = Some(now_rfc3339());
            r.report = Some(report);
            r.equity = Some(equity);
            r.trades = Some(trades);
        }
    }

    /// Transition a run to `Failed` with an error message (never key material).
    fn fail(&self, id: &str, err: String) {
        let mut g = self.lock();
        if let Some(r) = g.iter_mut().find(|r| r.id == id) {
            r.status = RunStatus::Failed;
            r.finished_at = Some(now_rfc3339());
            r.error = Some(err);
        }
    }

    fn summaries_newest_first(&self) -> Vec<RunSummary> {
        self.lock().iter().rev().map(RunSummary::from_run).collect()
    }

    fn get(&self, id: &str) -> Option<BacktestRun> {
        self.lock().iter().find(|r| r.id == id).cloned()
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

static RUN_SEQ: AtomicU64 = AtomicU64::new(1);

fn new_run_id() -> String {
    let seq = RUN_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("bt-{}-{}", chrono::Utc::now().timestamp_millis(), seq)
}

// ── Request body ─────────────────────────────────────────────────────────────

/// POST /api/backtest/run body — mirrors the CLI args. Numeric knobs are strings so
/// they round-trip exactly as Decimals (consistent with the rest of the API).
#[derive(Debug, Deserialize)]
pub struct RunRequest {
    pub coin: String,
    pub start: String,
    pub end: String,
    #[serde(default)]
    pub interval: Option<String>,
    #[serde(default)]
    pub spread: Option<String>,
    #[serde(default)]
    pub depth: Option<String>,
    #[serde(default)]
    pub commission: Option<String>,
    #[serde(default)]
    pub starting: Option<String>,
    #[serde(default)]
    pub strategies: Option<Vec<String>>,
    #[serde(default)]
    pub llm_score: Option<bool>,
    /// Historical-data provider: "hyperliquid" (default) | "binance". Both hit
    /// public, unauthenticated endpoints — there is no key to configure.
    #[serde(default)]
    pub source: Option<String>,
}

/// Parse an optional Decimal knob, falling back to `default` when empty/absent.
fn parse_dec(v: Option<&str>, default: &str, field: &str) -> anyhow::Result<Decimal> {
    let s = v.map(|s| s.trim()).filter(|s| !s.is_empty()).unwrap_or(default);
    Decimal::from_str(s).with_context(|| format!("invalid {field}: {s}"))
}

/// Resolve a request into a fully-validated `BacktestConfig`, applying the same
/// defaults as the CLI. The cache path is the CLI default (a separate sqlite file);
/// `out_dir` is unused because the API keeps results in the registry, never on disk.
fn build_config(req: &RunRequest) -> anyhow::Result<BacktestConfig> {
    let coin = req.coin.trim();
    if coin.is_empty() {
        bail!("coin is required (e.g. \"BTC\")");
    }
    let coin = coin.to_uppercase();
    let start_ms = parse_time(&req.start).context("start")?;
    let end_ms = parse_time(&req.end).context("end")?;
    if end_ms <= start_ms {
        bail!("end ({end_ms}) must be after start ({start_ms})");
    }
    let interval = req
        .interval
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "1m".to_string());
    let half_spread = parse_dec(req.spread.as_deref(), "0.02", "spread")?;
    let depth = parse_dec(req.depth.as_deref(), "500", "depth")?;
    let commission = parse_dec(req.commission.as_deref(), "0", "commission")?;
    let starting = parse_dec(req.starting.as_deref(), "500", "starting")?;

    // Range-check the numeric knobs. Prediction-market prices live in [0.01, 0.99], so a
    // negative half-spread would synthesize a CROSSED book (ask < bid) that banks phantom
    // profit on every round-trip, silently corrupting the "authoritative" ledger; guard it
    // (and the other physically-meaningless values) here since the harness does not.
    let half = Decimal::from_str("0.5").unwrap();
    let one = Decimal::from_str("1").unwrap();
    if half_spread < Decimal::ZERO || half_spread > half {
        bail!("spread (book half-spread) must be between 0 and 0.5, got {half_spread}");
    }
    if depth <= Decimal::ZERO {
        bail!("depth must be a positive number of shares, got {depth}");
    }
    if commission < Decimal::ZERO || commission >= one {
        bail!("commission rate must be in [0, 1), got {commission}");
    }
    if starting <= Decimal::ZERO {
        bail!("starting collateral must be positive, got {starting}");
    }
    let strategies = req
        .strategies
        .clone()
        .map(|v| {
            v.into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty());

    let source = match req.source.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => SourceKind::Hyperliquid,
        Some(s) => <SourceKind as clap::ValueEnum>::from_str(s, true)
            .map_err(|_| anyhow::anyhow!("invalid source '{s}' (valid: hyperliquid | binance)"))?,
    };

    Ok(BacktestConfig {
        crypto_filter: coin.to_lowercase(),
        coin,
        interval,
        start_ms,
        end_ms,
        strategies,
        half_spread,
        depth,
        commission,
        starting_collateral: starting,
        llm_score: req.llm_score.unwrap_or(false),
        cache_path: DEFAULT_CACHE_PATH.to_string(),
        out_dir: "backtest_out".to_string(),
        sigma_window: 60,
        source,
    })
}

fn params_echo(cfg: &BacktestConfig) -> RunParamsEcho {
    RunParamsEcho {
        coin: cfg.coin.clone(),
        interval: cfg.interval.clone(),
        start_ms: cfg.start_ms,
        end_ms: cfg.end_ms,
        spread: cfg.half_spread.to_string(),
        depth: cfg.depth.to_string(),
        commission: cfg.commission.to_string(),
        starting: cfg.starting_collateral.to_string(),
        strategies: cfg.strategies.clone(),
        llm_score: cfg.llm_score,
        source: cfg.source.as_str().to_string(),
    }
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// POST /api/backtest/run — validate, register a `Running` entry, and spawn the harness.
/// Returns 409 while another run is in progress, 400 on bad params, 202 on accept.
pub async fn run_backtest_handler(State(s): State<ApiState>, Json(req): Json<RunRequest>) -> Response {
    let registry = s.backtest_registry.clone();

    // Validate BEFORE claiming the single-run slot so bad params don't need a slot.
    let cfg = match build_config(&req) {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": e.to_string() }))).into_response();
        }
    };

    // One run at a time (rs-backtester process-global CONFIG). The busy-check and the
    // insert happen under a SINGLE MutexGuard inside `try_begin`, so two concurrent POSTs
    // cannot both pass the guard and spawn overlapping runs.
    let id = new_run_id();
    let admitted = registry.try_begin(BacktestRun {
        id: id.clone(),
        params: params_echo(&cfg),
        status: RunStatus::Running,
        error: None,
        started_at: now_rfc3339(),
        finished_at: None,
        report: None,
        equity: None,
        trades: None,
    });
    if !admitted {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "a backtest run is already in progress" })),
        )
            .into_response();
    }

    // Spawn the harness so the handler returns immediately; the UI polls for status.
    let reg = registry.clone();
    let run_id = id.clone();
    let handle = tokio::spawn(async move {
        match run_and_collect(&cfg).await {
            Ok(c) => reg.complete(&run_id, c.report, c.equity, c.trades),
            Err(e) => reg.fail(&run_id, e.to_string()),
        }
    });

    // Observe the JoinHandle: if the run task PANICS (the harness drives the real vipers
    // over synthetic replay inputs and is not panic-contained), neither `complete` nor
    // `fail` runs, leaving the entry stuck `Running` forever and wedging the 409 guard
    // until the LIVE process restarts. On panic we mark it `Failed` so the slot frees.
    let reg_watch = registry.clone();
    let watch_id = id.clone();
    tokio::spawn(async move {
        if let Err(join_err) = handle.await {
            if join_err.is_panic() {
                reg_watch.fail(&watch_id, "backtest task panicked".to_string());
            }
        }
    });

    (StatusCode::ACCEPTED, Json(json!({ "id": id, "status": "running" }))).into_response()
}

/// GET /api/backtest/runs — list run summaries, newest first.
pub async fn list_runs_handler(State(s): State<ApiState>) -> Response {
    Json(s.backtest_registry.summaries_newest_first()).into_response()
}

/// GET /api/backtest/runs/{id} — full run record (report + equity + trades + scores).
pub async fn get_run_handler(State(s): State<ApiState>, Path(id): Path<String>) -> Response {
    match s.backtest_registry.get(&id) {
        Some(run) => Json(run).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("run '{id}' not found") })),
        )
            .into_response(),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn echo() -> RunParamsEcho {
        RunParamsEcho {
            coin: "BTC".into(),
            interval: "1m".into(),
            start_ms: 0,
            end_ms: 1,
            spread: "0.02".into(),
            depth: "500".into(),
            commission: "0".into(),
            starting: "500".into(),
            strategies: None,
            llm_score: false,
            source: "hyperliquid".into(),
        }
    }

    fn running(id: &str) -> BacktestRun {
        BacktestRun {
            id: id.into(),
            params: echo(),
            status: RunStatus::Running,
            error: None,
            started_at: now_rfc3339(),
            finished_at: None,
            report: None,
            equity: None,
            trades: None,
        }
    }

    #[test]
    fn fresh_registry_is_idle() {
        let reg = BacktestRegistry::new();
        assert!(!reg.is_busy());
        assert!(reg.summaries_newest_first().is_empty());
        assert!(reg.get("nope").is_none());
    }

    #[test]
    fn insert_makes_it_busy() {
        let reg = BacktestRegistry::new();
        reg.insert(running("a"));
        assert!(reg.is_busy());
    }

    #[test]
    fn complete_transitions_and_clears_busy() {
        let reg = BacktestRegistry::new();
        reg.insert(running("a"));
        reg.complete("a", json!({"ok": true}), vec![], vec![]);
        assert!(!reg.is_busy());
        let run = reg.get("a").unwrap();
        assert_eq!(run.status, RunStatus::Done);
        assert!(run.report.is_some());
        assert!(run.finished_at.is_some());
        assert!(run.error.is_none());
    }

    #[test]
    fn fail_transitions_and_records_error() {
        let reg = BacktestRegistry::new();
        reg.insert(running("a"));
        reg.fail("a", "boom".into());
        assert!(!reg.is_busy());
        let run = reg.get("a").unwrap();
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.error.as_deref(), Some("boom"));
        assert!(run.report.is_none());
    }

    #[test]
    fn summaries_are_newest_first() {
        let reg = BacktestRegistry::new();
        reg.insert(running("first"));
        reg.insert(running("second"));
        let sums = reg.summaries_newest_first();
        assert_eq!(sums[0].id, "second");
        assert_eq!(sums[1].id, "first");
    }

    #[test]
    fn try_begin_admits_one_then_rejects_until_free() {
        let reg = BacktestRegistry::new();
        assert!(reg.try_begin(running("a")), "first run should be admitted");
        assert!(!reg.try_begin(running("b")), "second run must be rejected while busy");
        // Finishing the first frees the slot.
        reg.complete("a", json!({"ok": true}), vec![], vec![]);
        assert!(reg.try_begin(running("c")), "run admitted after the slot frees");
        assert!(reg.get("b").is_none(), "rejected run was never inserted");
    }

    #[test]
    fn prune_caps_retained_runs_and_keeps_running() {
        let reg = BacktestRegistry::new();
        // Seed more than MAX_RUNS finished entries.
        for i in 0..(MAX_RUNS + 10) {
            let id = format!("done-{i}");
            reg.insert(running(&id));
            reg.complete(&id, json!({}), vec![], vec![]);
        }
        assert!(reg.summaries_newest_first().len() <= MAX_RUNS, "registry is capped");
        // A running entry survives eviction even as new finished ones arrive.
        reg.insert(running("live"));
        for i in 0..MAX_RUNS {
            let id = format!("more-{i}");
            reg.insert(running(&id));
            reg.complete(&id, json!({}), vec![], vec![]);
        }
        assert!(reg.get("live").is_some(), "the in-flight run is never evicted");
    }

    #[test]
    fn build_config_rejects_insane_params() {
        let base = || RunRequest {
            coin: "BTC".into(),
            start: "1000000000".into(),
            end: "1000003600".into(),
            interval: None,
            spread: None,
            depth: None,
            commission: None,
            starting: None,
            strategies: None,
            llm_score: None,
            source: None,
        };
        let neg_spread = RunRequest { spread: Some("-0.02".into()), ..base() };
        assert!(build_config(&neg_spread).is_err(), "negative spread rejected");
        let big_spread = RunRequest { spread: Some("0.9".into()), ..base() };
        assert!(build_config(&big_spread).is_err(), "spread > 0.5 rejected");
        let bad_depth = RunRequest { depth: Some("0".into()), ..base() };
        assert!(build_config(&bad_depth).is_err(), "non-positive depth rejected");
        let bad_comm = RunRequest { commission: Some("1".into()), ..base() };
        assert!(build_config(&bad_comm).is_err(), "commission >= 1 rejected");
        let bad_start = RunRequest { starting: Some("0".into()), ..base() };
        assert!(build_config(&bad_start).is_err(), "non-positive starting rejected");
    }

    #[test]
    fn build_config_applies_cli_defaults() {
        let req = RunRequest {
            coin: "btc".into(),
            start: "1000000000".into(),
            end: "1000003600".into(),
            interval: None,
            spread: None,
            depth: None,
            commission: None,
            starting: None,
            strategies: None,
            llm_score: None,
            source: None,
        };
        let cfg = build_config(&req).unwrap();
        assert_eq!(cfg.coin, "BTC");
        assert_eq!(cfg.crypto_filter, "btc");
        assert_eq!(cfg.interval, "1m");
        assert_eq!(cfg.half_spread, Decimal::from_str("0.02").unwrap());
        assert_eq!(cfg.depth, Decimal::from_str("500").unwrap());
        assert_eq!(cfg.starting_collateral, Decimal::from_str("500").unwrap());
        assert!(!cfg.llm_score);
        assert_eq!(cfg.cache_path, DEFAULT_CACHE_PATH);
    }

    #[test]
    fn build_config_rejects_bad_window() {
        let req = RunRequest {
            coin: "BTC".into(),
            start: "1000003600".into(),
            end: "1000000000".into(),
            interval: None,
            spread: None,
            depth: None,
            commission: None,
            starting: None,
            strategies: None,
            llm_score: None,
            source: None,
        };
        assert!(build_config(&req).is_err());
    }

    #[test]
    fn build_config_defaults_to_hyperliquid() {
        let req = RunRequest {
            coin: "BTC".into(),
            start: "1000000000".into(),
            end: "1000003600".into(),
            interval: None,
            spread: None,
            depth: None,
            commission: None,
            starting: None,
            strategies: None,
            llm_score: None,
            source: None,
        };
        let cfg = build_config(&req).unwrap();
        assert_eq!(cfg.source, SourceKind::Hyperliquid);
    }

    #[test]
    fn build_config_accepts_binance_case_insensitive() {
        let base = |source: &str| RunRequest {
            coin: "BTC".into(),
            start: "1000000000".into(),
            end: "1000003600".into(),
            interval: None,
            spread: None,
            depth: None,
            commission: None,
            starting: None,
            strategies: None,
            llm_score: None,
            source: Some(source.into()),
        };
        let cfg = build_config(&base("binance")).unwrap();
        assert_eq!(cfg.source, SourceKind::Binance);
        let cfg = build_config(&base("BINANCE")).unwrap();
        assert_eq!(cfg.source, SourceKind::Binance);
    }

    #[test]
    fn build_config_rejects_unknown_source() {
        let req = RunRequest {
            coin: "BTC".into(),
            start: "1000000000".into(),
            end: "1000003600".into(),
            interval: None,
            spread: None,
            depth: None,
            commission: None,
            starting: None,
            strategies: None,
            llm_score: None,
            source: Some("kraken".into()),
        };
        assert!(build_config(&req).is_err(), "unknown source 'kraken' rejected");
    }
}
