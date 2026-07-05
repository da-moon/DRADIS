//! Live end-to-end smoke tests for the Anthropic LLM backend
//! (`src/helpers/llm_client.rs`) — `#[ignore]`-gated so they never run in
//! normal `cargo test` (they hit real network APIs and cost real tokens).
//!
//! Run manually with:
//!
//!     ANTHROPIC_API_KEY="$ANTHROPIC_API_KEY_SANDBOX" \
//!         cargo test --test llm_live -- --ignored --nocapture
//!
//! SECURITY: these tests read `ANTHROPIC_API_KEY` from the environment and
//! pass it straight into `LlmSettings`/`build_client()` (the same public path
//! production uses). The key value itself is NEVER printed, logged, formatted,
//! or otherwise written out by this file.

use dradis::helpers::llm_client::{build_client, LlmProvider, LlmSettings};

/// Model under test. If Anthropic ever rejects this id, retry with the alias
/// `"claude-haiku-4-5"` (see task notes) — kept as a fallback constant so a
/// human can flip it in one place.
const MODEL: &str = "claude-haiku-4-5-20251001";

/// Build an `LlmSettings` for the Anthropic provider through the exact same
/// struct + `build_client()` constructor production code uses. Returns `None`
/// (with an `eprintln!` skip message) when `ANTHROPIC_API_KEY` is unset so CI
/// never breaks on a missing secret.
fn anthropic_settings_from_env() -> Option<LlmSettings> {
    let api_key = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => {
            eprintln!(
                "SKIP: ANTHROPIC_API_KEY not set in the environment — skipping live Anthropic test"
            );
            return None;
        }
    };

    Some(LlmSettings {
        provider: LlmProvider::Anthropic,
        model: MODEL.to_string(),
        base_url: None,
        api_key: Some(api_key),
        temperature: 0.0,
        max_tokens: 32, // keep cost minimal — this is just a roundtrip probe
        timeout_secs: 30,
    })
}

#[tokio::test]
#[ignore = "hits live APIs; needs ANTHROPIC_API_KEY"]
async fn anthropic_live_chat_roundtrip() {
    let settings = match anthropic_settings_from_env() {
        Some(s) => s,
        None => return,
    };

    let client = build_client(&settings).expect("build_client(Anthropic) should succeed");

    // Sanity-check the model tag production code stores/renders — never
    // touches the key.
    let tag = client.model_tag();
    assert!(
        tag.starts_with("anthropic/"),
        "expected model_tag to look like 'anthropic/<model>', got {tag:?}"
    );

    let reply = client
        .chat("You are a terse test harness.", "Reply with exactly: PONG")
        .await
        .expect("anthropic chat() should succeed");

    assert!(
        !reply.trim().is_empty(),
        "expected a non-empty reply from Anthropic"
    );
    eprintln!("anthropic_live_chat_roundtrip: model_tag={tag} reply={reply:?}");
}

#[tokio::test]
#[ignore = "hits live APIs; needs ANTHROPIC_API_KEY"]
async fn hyperliquid_plus_anthropic_smoke() {
    let settings = match anthropic_settings_from_env() {
        Some(s) => s,
        None => return,
    };

    // ── Step 1: pull a real BTC candle from Hyperliquid (no cargo feature
    // needed — this is a plain reqwest POST to the public /info endpoint). ──
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock should be after the epoch")
        .as_millis() as i64;
    let start_ms = now_ms - 3 * 60 * 60 * 1000;

    let body = serde_json::json!({
        "type": "candleSnapshot",
        "req": {
            "coin": "BTC",
            "interval": "1h",
            "startTime": start_ms,
            "endTime": now_ms,
        }
    });

    let http = reqwest::Client::new();
    let resp = http
        .post("https://api.hyperliquid.xyz/info")
        .json(&body)
        .send()
        .await
        .expect("hyperliquid /info request failed");

    assert!(
        resp.status().is_success(),
        "hyperliquid /info returned HTTP {}",
        resp.status()
    );

    let candles: Vec<serde_json::Value> = resp
        .json()
        .await
        .expect("decoding hyperliquid candleSnapshot response");

    let last = candles
        .last()
        .expect("expected at least one candle in the response");
    let close_px = last
        .get("c")
        .and_then(|v| v.as_str())
        .expect("candle should have a string 'c' (close) field");

    // ── Step 2: feed the observed close price to the live Anthropic client. ──
    let client = build_client(&settings).expect("build_client(Anthropic) should succeed");

    let prompt = format!("BTC last close is {close_px}. Reply OK.");
    let reply = client
        .chat("You are a terse test harness.", &prompt)
        .await
        .expect("anthropic chat() should succeed");

    assert!(
        !reply.trim().is_empty(),
        "expected a non-empty reply from Anthropic"
    );
    eprintln!(
        "hyperliquid_plus_anthropic_smoke: close_px={close_px} reply={reply:?}"
    );
}
