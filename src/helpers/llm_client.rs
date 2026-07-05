//! Provider-neutral LLM chat client for the LLM Advisor.
//!
//! ── Why this module exists ────────────────────────────────────────────────────
//! The advisor historically spoke raw Ollama HTTP.  This module generalises that
//! into a small DRADIS-owned trait (`LlmChat`) with several backends:
//!
//!   • `ollama`             — local/remote Ollama (DEFAULT; behaviour preserved
//!                            byte-for-byte via the hand-rolled reqwest client below)
//!   • `anthropic`          — Claude models (ANTHROPIC_API_KEY)
//!   • `openai`             — OpenAI platform (OPENAI_API_KEY, Chat Completions API)
//!   • `openai-compatible`  — any OpenAI-shaped server (vLLM / LM Studio / OpenRouter /
//!                            Groq …) via a custom base URL + optional key
//!   • `chatgpt`            — "Sign in with ChatGPT" OAuth subscription backend
//!
//! ── Design rule ───────────────────────────────────────────────────────────────
//! ALL `rig-core` usage is confined to THIS file.  rig's 0.x API churn therefore
//! never leaks past the `LlmChat` trait — the advisor loop only ever sees
//! `Box<dyn LlmChat>`.  The Ollama backend deliberately does NOT go through rig:
//! rig's Ollama request body adds top-level `max_tokens` / `think` / duplicated
//! `temperature` fields that differ from our historical wire format, so to keep an
//! existing OLLAMA_URL/OLLAMA_MODEL deployment behaving IDENTICALLY we keep the
//! original hand-rolled reqwest client (same endpoints, same
//! num_ctx=3072 / num_predict=900 / temperature=0.3, same 5s/10s probe + 480s
//! inference timeouts).
//!
//! Every backend additionally wraps its network call in `tokio::time::timeout`
//! as defence-in-depth, regardless of any client-level timeout.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::time::timeout;
use tracing::{error, warn};

use rig_core::client::CompletionClient;
use rig_core::completion::{AssistantContent, CompletionModel};
use rig_core::providers::{anthropic, chatgpt, openai};

use crate::config;

// ── Trait ─────────────────────────────────────────────────────────────────────

/// One-shot, non-streaming chat completion: system + user -> assistant text.
///
/// Implementors own their own timeout enforcement so the advisor loop can treat
/// every provider uniformly.
#[async_trait]
pub trait LlmChat: Send + Sync {
    /// Run a single system+user completion and return the assistant's plain text.
    async fn chat(&self, system: &str, user: &str) -> Result<String>;

    /// Human-readable "provider/model" tag, stored in `llm_recommendations.model`
    /// and rendered as the Control Tower badge (e.g. "ollama/llama3.2",
    /// "anthropic/claude-3-5-sonnet-latest").
    fn model_tag(&self) -> String;

    /// Cheap reachability pre-flight.  Cloud APIs skip this (default `Ok(())`);
    /// Ollama overrides it with a `GET /api/tags` health check.
    async fn probe(&self) -> Result<()> {
        Ok(())
    }
}

// ── Provider enum ─────────────────────────────────────────────────────────────

/// Which LLM backend the advisor talks to.  Selected at runtime (no cargo feature).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmProvider {
    Ollama,
    Anthropic,
    OpenAi,
    OpenAiCompatible,
    ChatGpt,
}

impl LlmProvider {
    /// Canonical lowercase name, used as the first half of `model_tag`.
    pub fn as_str(&self) -> &'static str {
        match self {
            LlmProvider::Ollama => "ollama",
            LlmProvider::Anthropic => "anthropic",
            LlmProvider::OpenAi => "openai",
            LlmProvider::OpenAiCompatible => "openai-compatible",
            LlmProvider::ChatGpt => "chatgpt",
        }
    }

    /// Parse a provider name (case-insensitive, with a few aliases).
    ///
    /// An unrecognised value is NOT fatal: repo convention is degrade-don't-panic,
    /// so we log an `error!` listing the valid values and fall back to Ollama.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> LlmProvider {
        match s.trim().to_ascii_lowercase().as_str() {
            "ollama" => LlmProvider::Ollama,
            "anthropic" => LlmProvider::Anthropic,
            "openai" => LlmProvider::OpenAi,
            "openai-compatible" | "openai_compatible" | "compat" => LlmProvider::OpenAiCompatible,
            "chatgpt" | "chatgpt-oauth" | "openai-oauth" => LlmProvider::ChatGpt,
            other => {
                error!(
                    "🤖 LLM Advisor: unknown LLM_PROVIDER '{}' — valid values: \
                     ollama | anthropic | openai | openai-compatible | chatgpt. \
                     Falling back to ollama.",
                    other
                );
                LlmProvider::Ollama
            }
        }
    }
}

// ── Settings + resolution ─────────────────────────────────────────────────────

/// Fully-resolved connection settings for a single advisor run.
///
/// Resolution happens once at advisor start (NOT hot-reloaded), matching the
/// historical behaviour of the Ollama env vars.
pub struct LlmSettings {
    pub provider: LlmProvider,
    pub model: String,
    /// ollama + openai-compatible (+ optional openai / anthropic / chatgpt override).
    pub base_url: Option<String>,
    /// API key / access token (never logged).
    pub api_key: Option<String>,
    pub temperature: f32,
    pub max_tokens: u32,
    pub timeout_secs: u64,
}

/// Trim to `None` when empty/whitespace so an empty env var or config const does
/// NOT shadow a lower-priority source.
fn non_empty(s: &str) -> Option<String> {
    if s.trim().is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Ollama base URL precedence (back-compat is a hard requirement):
/// env `LLM_BASE_URL` → env `OLLAMA_URL` → config `LLM_BASE_URL` (if non-empty)
/// → config `LLM_OLLAMA_URL`.
fn resolve_ollama_base_url(
    env_llm_base_url: Option<String>,
    env_ollama_url: Option<String>,
    cfg_llm_base_url: &str,
    cfg_ollama_url: &str,
) -> String {
    env_llm_base_url
        .or(env_ollama_url)
        .or_else(|| non_empty(cfg_llm_base_url))
        .unwrap_or_else(|| cfg_ollama_url.to_string())
}

/// Ollama model precedence:
/// env `LLM_MODEL` → env `OLLAMA_MODEL` → config `LLM_MODEL` (if non-empty)
/// → config `LLM_OLLAMA_MODEL`.
fn resolve_ollama_model(
    env_llm_model: Option<String>,
    env_ollama_model: Option<String>,
    cfg_llm_model: &str,
    cfg_ollama_model: &str,
) -> String {
    env_llm_model
        .or(env_ollama_model)
        .or_else(|| non_empty(cfg_llm_model))
        .unwrap_or_else(|| cfg_ollama_model.to_string())
}

/// Non-ollama model precedence: env `LLM_MODEL` → config `LLM_MODEL` (if non-empty).
/// `None` here is a hard error (cloud providers have no sensible local default).
fn resolve_cloud_model(env_llm_model: Option<String>, cfg_llm_model: &str) -> Option<String> {
    env_llm_model.or_else(|| non_empty(cfg_llm_model))
}

impl LlmSettings {
    /// Resolve the advisor's LLM settings from env vars + compile-time config.
    ///
    /// Reads (once): `LLM_PROVIDER`, `LLM_MODEL`, `LLM_BASE_URL`, legacy
    /// `OLLAMA_URL` / `OLLAMA_MODEL`, and per-provider keys (`ANTHROPIC_API_KEY`,
    /// `OPENAI_API_KEY`, `CHATGPT_ACCESS_TOKEN`).
    pub fn resolve_from_env() -> Result<LlmSettings> {
        let env_opt =
            |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());

        let provider = match env_opt("LLM_PROVIDER") {
            Some(v) => LlmProvider::from_str(&v),
            None => LlmProvider::from_str(config::LLM_PROVIDER),
        };

        match provider {
            LlmProvider::Ollama => {
                let base_url = resolve_ollama_base_url(
                    env_opt("LLM_BASE_URL"),
                    env_opt("OLLAMA_URL"),
                    config::LLM_BASE_URL,
                    config::LLM_OLLAMA_URL,
                );
                let model = resolve_ollama_model(
                    env_opt("LLM_MODEL"),
                    env_opt("OLLAMA_MODEL"),
                    config::LLM_MODEL,
                    config::LLM_OLLAMA_MODEL,
                );
                Ok(LlmSettings {
                    provider,
                    model,
                    base_url: Some(base_url),
                    api_key: None,
                    temperature: 0.3,
                    max_tokens: 900,
                    timeout_secs: config::LLM_INFERENCE_TIMEOUT_SECS,
                })
            }
            _ => {
                let model = resolve_cloud_model(env_opt("LLM_MODEL"), config::LLM_MODEL)
                    .ok_or_else(|| {
                        anyhow!(
                            "provider '{}' requires a model name — set LLM_MODEL (env) \
                             or config::LLM_MODEL",
                            provider.as_str()
                        )
                    })?;

                let base_url = env_opt("LLM_BASE_URL").or_else(|| non_empty(config::LLM_BASE_URL));

                let api_key = match provider {
                    LlmProvider::Anthropic => env_opt("ANTHROPIC_API_KEY"),
                    LlmProvider::OpenAi | LlmProvider::OpenAiCompatible => env_opt("OPENAI_API_KEY"),
                    LlmProvider::ChatGpt => env_opt("CHATGPT_ACCESS_TOKEN"),
                    LlmProvider::Ollama => unreachable!(),
                };

                Ok(LlmSettings {
                    provider,
                    model,
                    base_url,
                    api_key,
                    temperature: 0.3,
                    max_tokens: 900,
                    timeout_secs: config::LLM_CLOUD_TIMEOUT_SECS,
                })
            }
        }
    }

    /// A display string for logs — base URL or a "(provider default)" placeholder.
    /// NEVER contains the API key.
    pub fn base_url_display(&self) -> String {
        self.base_url
            .clone()
            .unwrap_or_else(|| "(provider default)".to_string())
    }
}

// ── Constructor ───────────────────────────────────────────────────────────────

/// Build a boxed `LlmChat` client for the resolved settings.
///
/// Returns `Err` (degrade-don't-panic) if a required input is missing; the
/// advisor logs it and exits its task rather than crashing the process.
pub fn build_client(settings: &LlmSettings) -> Result<Box<dyn LlmChat>> {
    let tag = format!("{}/{}", settings.provider.as_str(), settings.model);

    match settings.provider {
        LlmProvider::Ollama => {
            let base = settings
                .base_url
                .clone()
                .unwrap_or_else(|| config::LLM_OLLAMA_URL.to_string());
            let client = OllamaClient::new(base, settings.model.clone(), settings.timeout_secs, tag)?;
            Ok(Box::new(client))
        }
        LlmProvider::Anthropic => {
            let key = settings
                .api_key
                .clone()
                .ok_or_else(|| anyhow!("anthropic provider requires ANTHROPIC_API_KEY"))?;
            let mut builder = anthropic::Client::builder().api_key(key);
            if let Some(base) = &settings.base_url {
                builder = builder.base_url(base);
            }
            let client = builder
                .build()
                .map_err(|e| anyhow!("failed to build anthropic client: {e}"))?;
            let model = client.completion_model(&settings.model);
            Ok(Box::new(RigChatClient::new(model, tag, settings)))
        }
        LlmProvider::OpenAi => {
            let key = settings
                .api_key
                .clone()
                .ok_or_else(|| anyhow!("openai provider requires OPENAI_API_KEY"))?;
            // Chat Completions API (not the Responses API) — the widely-supported shape.
            let mut builder = openai::CompletionsClient::builder().api_key(key);
            if let Some(base) = &settings.base_url {
                builder = builder.base_url(base);
            }
            let client = builder
                .build()
                .map_err(|e| anyhow!("failed to build openai client: {e}"))?;
            let model = client.completion_model(&settings.model);
            Ok(Box::new(RigChatClient::new(model, tag, settings)))
        }
        LlmProvider::OpenAiCompatible => {
            let base = settings.base_url.clone().ok_or_else(|| {
                anyhow!(
                    "openai-compatible provider requires a base URL — set LLM_BASE_URL \
                     (e.g. http://localhost:1234/v1)"
                )
            })?;
            // Many local servers need no key; rig still requires one, so use a dummy.
            let key = settings
                .api_key
                .clone()
                .unwrap_or_else(|| "not-needed".to_string());
            let client = openai::CompletionsClient::builder()
                .api_key(key)
                .base_url(&base)
                .build()
                .map_err(|e| anyhow!("failed to build openai-compatible client: {e}"))?;
            let model = client.completion_model(&settings.model);
            Ok(Box::new(RigChatClient::new(model, tag, settings)))
        }
        LlmProvider::ChatGpt => {
            // OAuth "Sign in with ChatGPT" subscription backend.
            //   • CHATGPT_ACCESS_TOKEN set  → explicit access-token auth
            //   • otherwise                 → OAuth (reads/refreshes rig's auth.json;
            //                                 default ~/.config/chatgpt/auth.json, or
            //                                 point CHATGPT_AUTH_FILE at another file)
            let account_id = std::env::var("CHATGPT_ACCOUNT_ID")
                .ok()
                .filter(|v| !v.trim().is_empty());
            let auth_file = std::env::var("CHATGPT_AUTH_FILE")
                .ok()
                .filter(|v| !v.trim().is_empty());

            let mut builder = chatgpt::Client::builder();
            if let Some(base) = &settings.base_url {
                builder = builder.base_url(base);
            }

            let keyed = match &settings.api_key {
                Some(token) => builder.api_key(chatgpt::ChatGPTAuth::AccessToken {
                    access_token: token.clone(),
                    account_id,
                }),
                None => builder.oauth(),
            };
            let keyed = match auth_file {
                Some(path) => keyed.auth_file(path),
                None => keyed,
            };
            let client = keyed
                .build()
                .map_err(|e| anyhow!("failed to build chatgpt client: {e}"))?;
            let model = client.completion_model(&settings.model);
            Ok(Box::new(RigChatClient::new(model, tag, settings)))
        }
    }
}

// ── rig-backed cloud client ───────────────────────────────────────────────────

/// Generic adapter that drives ANY rig `CompletionModel` (anthropic / openai /
/// openai-compatible / chatgpt) through the `LlmChat` trait.
struct RigChatClient<M: CompletionModel> {
    model: M,
    tag: String,
    temperature: f64,
    max_tokens: u64,
    timeout_secs: u64,
}

impl<M: CompletionModel> RigChatClient<M> {
    fn new(model: M, tag: String, settings: &LlmSettings) -> Self {
        Self {
            model,
            tag,
            temperature: settings.temperature as f64,
            max_tokens: settings.max_tokens as u64,
            timeout_secs: settings.timeout_secs,
        }
    }
}

#[async_trait]
impl<M> LlmChat for RigChatClient<M>
where
    M: CompletionModel + Send + Sync + 'static,
{
    async fn chat(&self, system: &str, user: &str) -> Result<String> {
        // preamble = system prompt, prompt/last message = user prompt.
        // (temperature / max_tokens are best-effort: some providers, e.g. the
        // chatgpt backend, intentionally ignore them.)
        let fut = self
            .model
            .completion_request(user.to_string())
            .preamble(system.to_string())
            .temperature(self.temperature)
            .max_tokens(self.max_tokens)
            .send();

        let resp = timeout(Duration::from_secs(self.timeout_secs), fut)
            .await
            .with_context(|| format!("LLM request timed out after {}s", self.timeout_secs))?
            .context("LLM completion request failed")?;

        // Extract plain text from the assistant choice (ignore tool/reasoning parts).
        let mut text = String::new();
        for content in resp.choice.iter() {
            if let AssistantContent::Text(t) = content {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&t.text);
            }
        }
        let text = text.trim().to_string();
        if text.is_empty() {
            return Err(anyhow!("LLM returned no text content"));
        }
        Ok(text)
    }

    fn model_tag(&self) -> String {
        self.tag.clone()
    }
}

// ── Ollama backend (hand-rolled reqwest — behaviour preserved) ─────────────────

#[derive(Serialize)]
struct OllamaRequest {
    model: String,
    messages: Vec<OllamaMessage>,
    stream: bool,
    /// Optional generation parameters — keep output focused and concise.
    options: OllamaOptions,
}

#[derive(Serialize)]
struct OllamaOptions {
    /// Allow fuller recommendations to be persisted to SQLite and shown in Control Tower.
    /// Telegram truncation is handled separately after generation.
    num_predict: u32,
    temperature: f32,
    /// Cap the KV-cache context window.  Smaller = faster prefill on CPU.
    /// 3072 leaves room for the prompt plus a longer recommendation.
    num_ctx: u32,
}

#[derive(Serialize, Deserialize, Clone)]
struct OllamaMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct OllamaResponse {
    message: OllamaMessage,
    done_reason: Option<String>,
}

/// Hand-rolled Ollama backend that reproduces the historical wire format exactly:
/// POST `{base}/api/chat` with `stream:false` and
/// `options:{num_predict:900, temperature:0.3, num_ctx:3072}`; probe via
/// GET `{base}/api/tags`.
struct OllamaClient {
    base_url: String,
    model: String,
    tag: String,
    timeout_secs: u64,
    /// Fast health-check client (5s connect / 10s total).
    probe_client: reqwest::Client,
    /// Inference client (10s connect / `timeout_secs` total).
    http_client: reqwest::Client,
}

impl OllamaClient {
    fn new(base_url: String, model: String, timeout_secs: u64, tag: String) -> Result<Self> {
        let probe_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()
            .context("building ollama probe client")?;
        let http_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .context("building ollama inference client")?;
        Ok(Self {
            base_url,
            model,
            tag,
            timeout_secs,
            probe_client,
            http_client,
        })
    }
}

#[async_trait]
impl LlmChat for OllamaClient {
    async fn chat(&self, system: &str, user: &str) -> Result<String> {
        let url = format!("{}/api/chat", self.base_url.trim_end_matches('/'));

        let request = OllamaRequest {
            model: self.model.clone(),
            messages: vec![
                OllamaMessage {
                    role: "system".to_string(),
                    content: system.to_string(),
                },
                OllamaMessage {
                    role: "user".to_string(),
                    content: user.to_string(),
                },
            ],
            stream: false,
            options: OllamaOptions {
                num_predict: 900,
                temperature: 0.3, // Low temperature: consistent, factual recommendations
                num_ctx: 3072,    // Room for prompt + full recommendation without frequent length stops
            },
        };

        let call = async {
            let resp = self
                .http_client
                .post(&url)
                .json(&request)
                .send()
                .await
                .context("ollama /api/chat request failed")?;

            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(anyhow!("Ollama HTTP {}: {}", status, body));
            }

            let ollama_resp: OllamaResponse =
                resp.json().await.context("decoding ollama /api/chat response")?;
            Ok::<OllamaResponse, anyhow::Error>(ollama_resp)
        };

        // Defence-in-depth timeout on top of the reqwest client-level timeout.
        let resp = timeout(Duration::from_secs(self.timeout_secs), call)
            .await
            .with_context(|| format!("ollama request timed out after {}s", self.timeout_secs))??;

        if matches!(resp.done_reason.as_deref(), Some("length")) {
            warn!(
                "🤖 LLM Advisor: output hit Ollama length cap (num_predict={}) — consider \
                 increasing if recommendations still end mid-thought",
                900,
            );
        }

        let content = resp.message.content.trim().to_string();
        if content.is_empty() {
            return Err(anyhow!("Ollama returned empty content"));
        }
        Ok(content)
    }

    fn model_tag(&self) -> String {
        self.tag.clone()
    }

    /// Quick reachability probe: GET /api/tags with a short timeout.
    async fn probe(&self) -> Result<()> {
        let url = format!("{}/api/tags", self.base_url.trim_end_matches('/'));
        let resp = self
            .probe_client
            .get(&url)
            .send()
            .await
            .context("ollama /api/tags probe failed")?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(anyhow!("Ollama /api/tags returned HTTP {}", resp.status()))
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str_canonical_and_aliases() {
        assert_eq!(LlmProvider::from_str("ollama"), LlmProvider::Ollama);
        assert_eq!(LlmProvider::from_str("anthropic"), LlmProvider::Anthropic);
        assert_eq!(LlmProvider::from_str("openai"), LlmProvider::OpenAi);
        assert_eq!(
            LlmProvider::from_str("openai-compatible"),
            LlmProvider::OpenAiCompatible
        );
        assert_eq!(
            LlmProvider::from_str("openai_compatible"),
            LlmProvider::OpenAiCompatible
        );
        assert_eq!(LlmProvider::from_str("compat"), LlmProvider::OpenAiCompatible);
        assert_eq!(LlmProvider::from_str("chatgpt"), LlmProvider::ChatGpt);
        assert_eq!(LlmProvider::from_str("chatgpt-oauth"), LlmProvider::ChatGpt);
        assert_eq!(LlmProvider::from_str("openai-oauth"), LlmProvider::ChatGpt);
    }

    #[test]
    fn from_str_is_case_insensitive_and_trims() {
        assert_eq!(LlmProvider::from_str("  Ollama  "), LlmProvider::Ollama);
        assert_eq!(LlmProvider::from_str("ANTHROPIC"), LlmProvider::Anthropic);
        assert_eq!(LlmProvider::from_str("OpenAI"), LlmProvider::OpenAi);
        assert_eq!(LlmProvider::from_str("ChatGPT"), LlmProvider::ChatGpt);
    }

    #[test]
    fn from_str_unknown_falls_back_to_ollama() {
        // Unknown values degrade to Ollama (never panic).
        assert_eq!(LlmProvider::from_str("gpt-9000"), LlmProvider::Ollama);
        assert_eq!(LlmProvider::from_str(""), LlmProvider::Ollama);
    }

    #[test]
    fn model_tag_format_is_provider_slash_model() {
        assert_eq!(
            format!("{}/{}", LlmProvider::Ollama.as_str(), "llama3.2"),
            "ollama/llama3.2"
        );
        assert_eq!(
            format!(
                "{}/{}",
                LlmProvider::Anthropic.as_str(),
                "claude-3-5-sonnet-latest"
            ),
            "anthropic/claude-3-5-sonnet-latest"
        );
        assert_eq!(
            format!("{}/{}", LlmProvider::OpenAiCompatible.as_str(), "qwen2.5"),
            "openai-compatible/qwen2.5"
        );
    }

    // ── Ollama back-compat resolution matrix ─────────────────────────────────
    // Config defaults mirror the shipped values.
    const CFG_OLLAMA_URL: &str = "http://localhost:11434";
    const CFG_OLLAMA_MODEL: &str = "llama3.2";
    // New generic consts default to empty ("" = fall back to the ollama consts).
    const CFG_LLM_BASE_URL: &str = "";
    const CFG_LLM_MODEL: &str = "";

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn base_url_legacy_only_ollama_url() {
        // Existing deployment: only OLLAMA_URL set → must be honoured verbatim.
        let base = resolve_ollama_base_url(
            None,
            s("http://gpu-box:11434"),
            CFG_LLM_BASE_URL,
            CFG_OLLAMA_URL,
        );
        assert_eq!(base, "http://gpu-box:11434");
    }

    #[test]
    fn base_url_nothing_set_uses_ollama_config_default() {
        let base = resolve_ollama_base_url(None, None, CFG_LLM_BASE_URL, CFG_OLLAMA_URL);
        assert_eq!(base, CFG_OLLAMA_URL);
    }

    #[test]
    fn base_url_new_env_wins_over_legacy() {
        let base = resolve_ollama_base_url(
            s("http://new:1"),
            s("http://legacy:2"),
            CFG_LLM_BASE_URL,
            CFG_OLLAMA_URL,
        );
        assert_eq!(base, "http://new:1");
    }

    #[test]
    fn base_url_config_llm_base_url_beats_ollama_default() {
        let base =
            resolve_ollama_base_url(None, None, "http://cfg-generic:9", CFG_OLLAMA_URL);
        assert_eq!(base, "http://cfg-generic:9");
    }

    #[test]
    fn base_url_legacy_env_beats_config_llm_base_url() {
        // env OLLAMA_URL outranks config::LLM_BASE_URL per the precedence table.
        let base = resolve_ollama_base_url(
            None,
            s("http://legacy-env:2"),
            "http://cfg-generic:9",
            CFG_OLLAMA_URL,
        );
        assert_eq!(base, "http://legacy-env:2");
    }

    #[test]
    fn model_legacy_only_ollama_model() {
        let model =
            resolve_ollama_model(None, s("mistral"), CFG_LLM_MODEL, CFG_OLLAMA_MODEL);
        assert_eq!(model, "mistral");
    }

    #[test]
    fn model_nothing_set_uses_ollama_config_default() {
        let model = resolve_ollama_model(None, None, CFG_LLM_MODEL, CFG_OLLAMA_MODEL);
        assert_eq!(model, CFG_OLLAMA_MODEL);
    }

    #[test]
    fn model_new_env_wins_over_legacy() {
        let model =
            resolve_ollama_model(s("qwen2.5"), s("mistral"), CFG_LLM_MODEL, CFG_OLLAMA_MODEL);
        assert_eq!(model, "qwen2.5");
    }

    #[test]
    fn model_config_llm_model_beats_ollama_default() {
        let model = resolve_ollama_model(None, None, "phi3", CFG_OLLAMA_MODEL);
        assert_eq!(model, "phi3");
    }

    #[test]
    fn cloud_model_requires_a_value() {
        // No env, empty config → None (advisor turns this into a hard error).
        assert_eq!(resolve_cloud_model(None, ""), None);
        // env wins
        assert_eq!(resolve_cloud_model(s("claude-x"), ""), s("claude-x"));
        // config fallback
        assert_eq!(resolve_cloud_model(None, "gpt-x"), s("gpt-x"));
        // env beats config
        assert_eq!(resolve_cloud_model(s("claude-x"), "gpt-x"), s("claude-x"));
    }

    #[test]
    fn empty_strings_are_treated_as_unset() {
        assert_eq!(non_empty(""), None);
        assert_eq!(non_empty("   "), None);
        assert_eq!(non_empty("x"), s("x"));
    }
}
