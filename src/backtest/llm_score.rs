//! W7 — Experimental LLM decision-scoring (off by default; `--llm-score`).
//!
//! At each Entry signal the decision context (viper, direction, key snapshot gate
//! values, intended size/price) is serialized into a compact prompt and sent to the
//! existing multi-provider LLM client (`helpers::llm_client::build_client`) for a
//! 0–100 conviction score + one-line rationale. Responses are cached in the backtest
//! SQLite keyed by a SHA-1 content hash, so reruns cost nothing and are reproducible.
//! Scores are later joined with realized per-trade PnL in `report.json`.
//!
//! Provider/model/keys come from the SAME env/config the live advisor uses
//! (`LlmSettings::resolve_from_env`). API keys are NEVER logged. Requests are issued
//! sequentially (one awaited call per Entry).

use anyhow::Result;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use sha1::{Digest, Sha1};
use sqlx::{Row, SqlitePool};
use tracing::{debug, warn};

use crate::helpers::llm_client::{build_client, LlmChat, LlmSettings};
use crate::state::{MarketSnapshot, OrderParams};

/// One scored entry decision, joined to its realized outcome after the run.
#[derive(Debug, Clone, Serialize)]
pub struct ScoredEntry {
    pub strategy: String,
    /// "YES" or "NO".
    pub side: String,
    pub entry_ts: DateTime<Utc>,
    /// 0–100 conviction.
    pub score: i32,
    pub rationale: String,
    /// Realized PnL of the matched trade (filled in after the run).
    pub realized_pnl: Option<Decimal>,
}

pub struct LlmScorer {
    client: Box<dyn LlmChat>,
    pool: SqlitePool,
    model_tag: String,
}

const SYSTEM: &str = "You are a quantitative trading decision scorer for a binary \
prediction market (Polymarket YES/NO shares on hourly crypto up/down markets). Given \
one entry decision's context, judge how convinced you are it is a good entry. Respond \
EXACTLY in two lines and nothing else:\nSCORE: <integer 0-100>\nRATIONALE: <one short line>";

impl LlmScorer {
    /// Build the scorer from the live advisor's env/config and ensure the cache table
    /// exists in the shared backtest DB. Returns `Err` (caller degrades to no scoring)
    /// if no provider is configured.
    pub async fn new(pool: SqlitePool) -> Result<Self> {
        let settings = LlmSettings::resolve_from_env()?;
        let client = build_client(&settings)?;
        let model_tag = client.model_tag();
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS llm_scores (
                hash TEXT PRIMARY KEY,
                score INTEGER NOT NULL,
                rationale TEXT NOT NULL
            )",
        )
        .execute(&pool)
        .await?;
        Ok(Self { client, pool, model_tag })
    }

    /// Score one entry decision, using the cache when available.
    pub async fn score_entry(
        &mut self,
        strategy: &str,
        is_yes: bool,
        wall_now: DateTime<Utc>,
        snap: &MarketSnapshot,
        params: &OrderParams,
    ) -> Option<ScoredEntry> {
        let side = if is_yes { "YES" } else { "NO" };
        let user = format!(
            "{{\"model\":\"{}\",\"viper\":\"{}\",\"direction\":\"{}\",\"oracle\":{},\
\"velocity\":{},\"velocity_1s\":{},\"accel\":{},\"drift_10m\":{},\"drift_60m\":{},\
\"funding\":{},\"secs_to_expiry\":{},\"yes_ask\":{},\"no_ask\":{},\"size_shares\":{},\
\"price\":{}}}",
            self.model_tag,
            strategy,
            side,
            snap.oracle_price,
            snap.velocity,
            snap.velocity_1s,
            snap.acceleration,
            snap.oracle_drift_10m,
            snap.oracle_drift_60m,
            snap.funding_rate,
            snap.secs_to_expiry,
            snap.yes_ask,
            snap.no_ask,
            params.shares.round_dp(2),
            params.price,
        );

        let hash = sha1_hex(&format!("{SYSTEM}\n{user}"));

        // Cache hit?
        if let Ok(Some(row)) = sqlx::query("SELECT score, rationale FROM llm_scores WHERE hash=?")
            .bind(&hash)
            .fetch_optional(&self.pool)
            .await
        {
            let score: i64 = row.get("score");
            let rationale: String = row.get("rationale");
            debug!("🤖 LLM score cache hit ({strategy} {side}): {score}");
            return Some(ScoredEntry {
                strategy: strategy.to_string(),
                side: side.to_string(),
                entry_ts: wall_now,
                score: score as i32,
                rationale,
                realized_pnl: None,
            });
        }

        // Live call (sequential; API key never logged).
        let (score, rationale) = match self.client.chat(SYSTEM, &user).await {
            Ok(text) => parse_score(&text),
            Err(e) => {
                warn!("🤖 LLM scoring call failed ({strategy} {side}): {e}");
                return None;
            }
        };

        let _ = sqlx::query("INSERT OR REPLACE INTO llm_scores (hash, score, rationale) VALUES (?,?,?)")
            .bind(&hash)
            .bind(score as i64)
            .bind(&rationale)
            .execute(&self.pool)
            .await;

        Some(ScoredEntry {
            strategy: strategy.to_string(),
            side: side.to_string(),
            entry_ts: wall_now,
            score,
            rationale,
            realized_pnl: None,
        })
    }
}

fn sha1_hex(s: &str) -> String {
    let mut h = Sha1::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Parse "SCORE: <n>\nRATIONALE: <line>" leniently. Score clamped to [0, 100].
fn parse_score(text: &str) -> (i32, String) {
    let mut score = 50;
    let mut rationale = String::new();
    for line in text.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("SCORE:").or_else(|| l.strip_prefix("Score:")) {
            // First whitespace-delimited token, allowing a leading sign.
            if let Some(tok) = rest.split_whitespace().next() {
                if let Ok(n) = tok.trim_matches(|c: char| !c.is_ascii_digit() && c != '-').parse::<i32>() {
                    score = n.clamp(0, 100);
                }
            }
        } else if let Some(rest) = l.strip_prefix("RATIONALE:").or_else(|| l.strip_prefix("Rationale:")) {
            rationale = rest.trim().to_string();
        }
    }
    if rationale.is_empty() {
        rationale = text.trim().lines().next().unwrap_or("").to_string();
    }
    (score, rationale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_is_stable() {
        assert_eq!(sha1_hex("abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn parses_well_formed_response() {
        let (s, r) = parse_score("SCORE: 73\nRATIONALE: strong upward drift confirmed");
        assert_eq!(s, 73);
        assert_eq!(r, "strong upward drift confirmed");
    }

    #[test]
    fn clamps_and_defaults() {
        assert_eq!(parse_score("SCORE: 250\nRATIONALE: x").0, 100);
        assert_eq!(parse_score("SCORE: -5\nRATIONALE: x").0, 0);
        // Missing score defaults to 50; rationale falls back to first line.
        let (s, r) = parse_score("no structured output here");
        assert_eq!(s, 50);
        assert_eq!(r, "no structured output here");
    }
}
