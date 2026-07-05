# DRADIS

> **Direct Reaction And Dynamic Intelligence System** — Low-latency Rust prediction-market trading bot for Polymarket. Eight autonomous Viper strategies, a Raptor recon layer (Price, Funding, Derivatives, and Tide "Institutional Pulse" scouts), a Squadron deployment framework, a CAG async dispatch layer with concurrent multi-asset support, a real-time Next.js Control Tower, and an LLM Advisor that delivers optimization recommendations via Ollama (local or remote) + Telegram & OpenClaw.

![Rust](https://img.shields.io/badge/Rust-1.95+-orange?logo=rust&logoColor=white)
![Tokio](https://img.shields.io/badge/Tokio-async%20runtime-darkgreen?logo=rust&logoColor=white)
![axum](https://img.shields.io/badge/axum-REST%20API-blue?logo=rust&logoColor=white)
![Next.js](https://img.shields.io/badge/Next.js-15-black?logo=next.js&logoColor=white)
![Tailwind CSS](https://img.shields.io/badge/Tailwind-CSS-38bdf8?logo=tailwindcss&logoColor=white)
![Node.js](https://img.shields.io/badge/Node.js-20-brightgreen?logo=node.js&logoColor=white)
![Ollama](https://img.shields.io/badge/Ollama-LLM%20Advisor-blueviolet?logo=ollama&logoColor=white)
![Docker](https://img.shields.io/badge/Docker-compose-2496ED?logo=docker&logoColor=white)
[![OpenClaw](https://img.shields.io/badge/OpenClaw-AI%20Integration-CC0000?logoColor=white)](https://openclaw.ai)
![License](https://img.shields.io/badge/License-GPLv3-blue)

**WARNING**: This is **ALPHA** software. You will probably lose money. Start in GHOST mode and tune before going live. Make sure to regularly pull updates as our own LLM advises on config and Viper strategy impls daily.

Public Demo Site: https://dradis.live/


---


## ️ Tactical Overview

DRADIS is a comprehensive trading automation platform for prediction markets. Built in Rust for maximum concurrency and memory safety, it evaluates selected markets every 50ms, coordinating multiple autonomous strategies to preserve capital and place orders where it sees inefficiencies.

The system is organized around four BSG-inspired tactical layers:

| Layer        | Folder          | Role                                                                           |
|--------------|-----------------|--------------------------------------------------------------------------------|
| **Raptors**  | `src/raptors/`  | Signal scouts — fetch, normalise, broadcast external data                      |
| **Vipers**   | `src/vipers/`   | Trading strategies — evaluate signals and place orders                         |
| **Squadron** | `src/squadron/` | Deployment unit — bundles Raptors + Vipers onto a battle location              |
| **CAG**      | `src/cag/`      | Commander Air Group — async dispatch, session state, multi-asset orchestration |


---

## ⚡ Quick Start

```bash
# 1. Clone and configure
git clone https://github.com/youruser/dradis.git && cd dradis
cp .env.example .env          # fill in POLYMARKET_PRIVATE_KEY, POLYGON_RPC_URL, TELEGRAM tokens, etc.
cp deploy-multi.sh.example deploy-multi.sh  # fill in HOST, USER, KEY
# choose one config profile and copy it into src/config.rs before building
cp src/config.balanced.rs.example src/config.rs   # or conservative/aggressive
```

```bash
# 2. Start locally (builds Rust engine + Control Tower)
./start-local.sh                  # Intl CLOB, BTC (default)
./start-local.sh eth              # Intl CLOB, ETH
VENUE=us ./start-local.sh        # US Retail venue (us_retail build)
RUST_LOG=debug ./start-local.sh  # verbose logging

tail -f logs/dradis-local.log
./stop-local.sh
```

After ~5 minutes the stack is live:

| Service             | URL                                       |
|---------------------|-------------------------------------------|
| **Control Tower**   | `http://<host>:3002`                      |
| **DRADIS REST API** | `http://<host>:9000/api/health`           |
| **Ollama**          | `http://<host>:11434/api/tags` (internal) |

> **Prerequisites:** Docker on the remote host, Rust 1.95+ only needed for local builds.

---

##  Choosing a venue (Intl CLOB vs US Retail)

DRADIS compiles for **exactly one** execution venue, chosen at build time via a Cargo
feature. Both share the same strategy/abstraction layers through the venue-neutral
`Execution` trait (`src/venues/core.rs`) and the shared `OrderLifecycle` reconciler
(`src/venues/lifecycle.rs`); only the venue module differs, so the unused venue's
dependencies are stripped from the binary.

| Feature              | Venue                              | Auth                                   | Gateway                              |
|----------------------|------------------------------------|----------------------------------------|--------------------------------------|
| `intl_clob` *(default)* | Polymarket International (self-custody) | EOA wallet + EIP-712 over Polygon      | `clob.polymarket.com`                |
| `us_retail`          | Polymarket US (custodial, CFTC)    | Ed25519 challenge-response → JWT        | `api.prod.polymarketexchange.com`    |

### Start locally

```bash
# Intl CLOB (default)
./start-local.sh                  # BTC
./start-local.sh eth              # ETH

# US Retail
VENUE=us ./start-local.sh
```

### Build manually

```bash
# International CLOB (default)
cargo build --release
cargo test

# US Retail
cargo build  --release --no-default-features --features us_retail
cargo test            --no-default-features --features us_retail
```

### US Retail configuration (`.env`)

```bash
POLYMARKET_US_KEY_ID=<key-id-uuid>      # developer-portal Key ID (X-PM-Access-Key)
POLYMARKET_US_SECRET_KEY=<base64-secret> # portal Secret Key (Base64 Ed25519 keypair), shown once
# optional:
POLYMARKET_US_BASE_URL=https://api.prod.polymarketexchange.com  # override (staging/mock)
POLYMARKET_US_TRADE_SIZE=10        # contracts per leg          (default 10)
POLYMARKET_US_ARB_EDGE=0.02        # min risk-free edge per pair (default $0.02)
POLYMARKET_US_MARKET_FILTER=chiefs # optional slug/question substring to pick a market
ASSETS=us                          # keep the dashboard pool tidy (US data lives in logs/us-dradis.db)
```

> **US Retail status:** the MVP loop (`src/venues/us/trader.rs`) runs the venue-agnostic
> **arbitrage** strategy — discover a binary market → stream both legs over WebSocket →
> buy `YES`+`NO` for < $1 via an **engine-atomic** batched order (`/v1/orders/batched`) →
> reconcile via `OrderLifecycle`. Open positions and portfolio P&L appear in the Control Tower under the **`us`**
> asset selector. The Control Tower API stays live on `:9000` regardless. Crypto-hourly
> strategies (Momentum/Maker/GBoost) remain intl-only for now.

---

## ️ Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                         src/ layout                                 │
│                                                                     │
│  raptors/          ← Signal scouts (Binance or Hyperliquid)         │
│  vipers/           ← Trading strategies (8 Vipers)                  │
│  squadron/         ← Deployment layer (Raptor+Viper+Market bundle)  │
│  cag/              ← Commander (async dispatch, multi-asset)        │
│  orchestrator/     ← Strategy trait, registry, executor             │
│  tasks/            ← Market monitor, cleanup, chain-sync            │
│  helpers/          ← DB, orders, balance, metrics, notifications    │
│  api/              ← axum REST server (:9000)                       │
└─────────────────────────────────────────────────────────────────────┘
```

```
┌──────────────────────┐   ┌──────────────────────┐
│    src/raptors/      │   │  Polymarket CLOB     │
│  Price Raptor        │   │  (WebSocket Feed)    │
│  (Binance/Hyperliq)  │   │                      │
│  Funding Raptor      │   │                      │
│  (Binance/Hyperliq)  │   │                      │
│  Derivatives Raptor  │   │                      │
│  (Binance/Hyperliq)  │   │                      │
│  Tide Raptor         │   │                      │
│  (Alpaca IEX + iNAV) │   │                      │
└──────────┬───────────┘   └───────────┬──────────┘
           │  watch channels           │ orderbook WS
           └─────────────┬─────────────┘
                         ▼
           ┌─────────────────────────┐
           │   src/cag/              │  ← CAG (Commander Air Group)
           │   run_market_loop()     │  ← one tokio task per asset
           │   SessionState          │  ← per-asset P&L + collateral
           └─────────────┬───────────┘
                         │  (BTC task) (ETH task) (SOL task) …
                         ▼
           ┌─────────────────────────┐
           │   src/squadron/         │
           │   Squadron descriptor   │  ← SquadronRaptors (signal bundle)
           │   (battle location +    │  ← SquadronConfig  (which Vipers fly)
           │   Raptor+Viper bundle)  │  ← SquadronState   (STAGED→PATROLLING→RTB)
           └─────────────┬───────────┘
                         ▼
           ┌─────────────────────────┐
           │   Orchestrator (CIC)    │◄──── axum REST API (:9000)
           │     50ms Heartbeat      │
           └─────────────┬───────────┘
                         │  parallel dispatch
           ┌─────────────┼───────────────┬───────────────┬──────────────┐
           ▼             ▼               ▼               ▼              ▼
    ┌────────────┐ ┌──────────┐ ┌───────────────┐ ┌────────────┐ ┌──────────────┐
    │ Momentum   │ │  Maker   │ │  Arbitrage /  │ │   GBoost   │ │ TrendCapture │
    │   Viper    │ │  Viper   │ │  TimeDecay /  │ │    Viper   │ │    Viper     │
    │            │ │          │ │  Basis Vipers │ │   (ML)     │ │ (drift/trend)│
    └──────┬─────┘ └────┬─────┘ └──────┬────────┘ └─────┬──────┘ └──────┬───────┘
           └────────────┼──────────────┴────────────────┴───────────────┘
                        ▼
           ┌───────────────────────┐
           │    Execution Layer    │
           │  OBI Gate · Fee Gate  │
           │  Circuit Breaker      │
           └───────────────────────┘

           ┌───────────────────────┐
           │   Control Tower UI    │  Next.js dashboard (:3002)
           │  Viper toggles        │  ◄── PATCH /api/config
           │  P&L chart            │  ◄── GET  /api/pnl/history
           │  Open Positions       │  ◄── GET  /api/positions
           │  Trade log            │  ◄── GET  /api/trades
           └───────────────────────┘

           ┌───────────────────────┐     ┌────────────────┐
           │    LLM Advisor        │────►│  Ollama API    │
           │  (background task)    │     │  (your model)  │
           └──────────┬────────────┘     └────────────────┘
                      ▼
           ┌───────────────────────┐
           │   Telegram Channel    │
           └───────────────────────┘
```

### Core design principles

- **Parallel Dispatch**: Every 50ms heartbeat, the CIC evaluates all registered Vipers concurrently.
- **Isolated budgets**: Each Viper has its own independent capital budget and position book — a loss in one sector can't drain another's fuel.
- **Multi-asset concurrency**: Each asset runs in its own `tokio::spawn`ed task with independent raptors and session state. The runtime uses 8 worker threads to cover BTC + ETH + SOL comfortably.
- **OS-thread watchdog**: A native OS thread (outside the tokio runtime) checks an atomic heartbeat every 60 s. If the trading loop goes silent for 5 minutes, it calls `process::exit(1)` to trigger Docker's restart policy — immune to tokio runtime deadlocks.
- **OBI Veto**: A built-in Order Book Imbalance gate at −0.60 blocks entries into toxic flow / distribution walls.
- **Strategy Timeout**: Each Viper evaluation is hard-capped at 500ms. A hung Viper is skipped for that tick — the engine never freezes.
- **REST API**: axum server on `:9000` exposes live config, P&L, positions, and trade history to the Control Tower.

---

##  Raptor Wing (`src/raptors/`)

Raptors are DRADIS's recon layer — lightweight signal scouts that fly ahead of the Vipers and report external intelligence back to the CIC. Each Raptor polls a specific data source on its own schedule and publishes a normalized signal via `watch` channels.

Raptors are intentionally dumb: **fetch, normalize, broadcast** — no trading logic, no position awareness, no side effects.

| Raptor                         | Source                  | Signal                                                  | Module                   |
|--------------------------------|-------------------------|---------------------------------------------------------|--------------------------|
| **Price Raptor**               | Binance or Hyperliquid (`MARKET_DATA_SOURCE`) | spot price, 5s/1s velocity, acceleration, 10m/60m drift | `src/raptors/price.rs`   |
| **Funding Raptor**             | Binance or Hyperliquid (`MARKET_DATA_SOURCE`) | Perpetual funding rate (smart-money sentiment)          | `src/raptors/funding.rs` |
| **Derivatives Raptor**         | Binance or Hyperliquid (`MARKET_DATA_SOURCE`) | Open-interest delta + taker CVD ratio (positioning pressure, all-asset) | `src/raptors/derivatives.rs` |
| **Tide Raptor**                | Alpaca IEX + synthetic iNAV | "Institutional Pulse" + coherence from spot-BTC-ETF (IBIT/FBTC/ARKB) premium vs iNAV — BTC-only, US-hours | `src/raptors/tide.rs` |
| *(future)* **Sports Raptor**   | Line movement APIs      | Betting line drift, public money %                      | —                        |
| *(future)* **Politics Raptor** | Polling aggregators     | Approval drift, event probability shifts                | —                        |

When multiple Raptors are active, the GBoost Viper fuses every signal as model features (funding, OI/CVD, institutional pulse/coherence); Basis, Momentum and TrendCapture use them as confirmation gates; and the **Convergence** Viper opens directional positions only when the institutional + derivatives stack agrees. No single Raptor has veto power alone.

### Market data source: Binance or Hyperliquid

The Price / Funding / Derivatives Raptors read from a runtime-selectable **market-data source**. The default is Binance; set `MARKET_DATA_SOURCE=hyperliquid` (env var, or `MARKET_DATA_SOURCE` in `src/config.rs`) to feed the raptor layer from the [Hyperliquid](https://hyperliquid.xyz) public Info API instead. This is a **data source only** — no keys, no signing, no trading; execution stays on Polymarket.

- **Enable:** `MARKET_DATA_SOURCE=hyperliquid` on the default build (the `hyperliquid` cargo feature ships in `default`). On a build compiled without that feature the engine logs an error and falls back to Binance — it never panics.
- **What maps to what:** one WS raptor per asset replaces the Binance trio. `Trades{coin}` → oracle price + 5s/1s velocity + acceleration + 10m/60m drift + rolling taker **CVD**; `ActiveAssetCtx{coin}` → funding rate (Hyperliquid quotes an **hourly** rate, so DRADIS emits it **×8** to match Binance's per-8h `lastFundingRate` semantics that the viper thresholds are tuned on) + **open interest** (sampled on the same 30s cadence as the Binance OI delta).
- **Resolution-source caveat (read this):** Polymarket's crypto "Up or Down" markets typically **resolve on Binance prices**. Running `MARKET_DATA_SOURCE=hyperliquid` means the oracle and strike-price references DRADIS trades against may deviate slightly from the market's actual resolution source. This is an intentional operator trade-off — useful e.g. in regions where Binance is unreachable — not a free swap. Prefer Binance unless you have a specific reason.

---

## ✈️ Viper Wing (`src/vipers/`)

Eight specialized Viper strategy classes. Each Viper is an autonomous tactical unit with its own capital budget, position book, and entry/exit logic.

| Viper            | Venue        | Description                                                                                                                                 |
|------------------|--------------|---------------------------------------------------------------------------------------------------------------------------------------------|
| **Momentum**     | Hourly       | Detects high-velocity Binance moves and strikes Polymarket before it reprices                                                               |
| **Maker**        | Window       | Dual-sided passive bids on YES+NO, capturing the spread while managing net exposure                                                         |
| **Arbitrage**    | Window/Daily | Buys both YES+NO when combined asks are < $1.00 (net of fees)                                                                               |
| **Time Decay**   | Hourly       | Posts resting GTC maker bids during the theta window; settles at $1.00 at 0% fee                                                            |
| **Basis**        | Window       | Fades retail skew using Binance funding rates as smart-money confirmation                                                                   |
| **GBoost**       | Window/Daily | Online gradient-boosted ML model retraining continuously on live orderbook + Raptor features                                                |
| **TrendCapture** | Window/Daily | Exploits sustained multi-minute oracle drift (10m + 60m) before Polymarket reprices; Kelly-fractional sizing, OBI veto, trend-reversal exit |
| **Convergence**  | Hourly       | Macro-conviction directional Viper — opens YES/NO only when the Tide institutional pulse, Derivatives CVD, and OI all agree on a direction. BTC-only, US-cash-hours-only, fixed tiny size |

Build your own: [CUSTOM_STRATEGY.md](docs/CUSTOM_STRATEGY.md).

---

## ️ Squadron Layer (`src/squadron/`)

A **Squadron** is the core deployable unit — it bundles Raptors with Vipers and sends them to a specific Polymarket market (the **battle location**).

```
Squadron
├── Battle Location  →  MarketConfig (yes/no tokens, expiry, fees)
├── SquadronRaptors  →  typed bundle of Raptor watch::Receiver handles
├── SquadronConfig   →  RaptorProfile + ViperProfile composition spec
└── SquadronState    →  STAGED → DEPLOYED → PATROLLING → RTB → STOOD_DOWN
```

### Composition presets

| Preset          | Raptors         | Vipers                             |
|-----------------|-----------------|------------------------------------|
| `full_wing`     | Price + Funding + Derivatives + Tide | All eight Vipers (current default) |
| `momentum_only` | Price only      | Momentum + GBoost                  |
| `arb_wing`      | Price + Funding | Arbitrage + Basis                  |

### Lifecycle states

| State        | Meaning                                          |
|--------------|--------------------------------------------------|
| `STAGED`     | Assembled, waiting for a battle location         |
| `DEPLOYED`   | Market acquired, WS subscriptions live           |
| `PATROLLING` | Active trading tick loop running                 |
| `RTB`        | Returning to base — no new entries, winding down |
| `STOOD_DOWN` | Market expired or manually stood down            |

Each market rotation logs: `️ Squadron [btc-hourly-2026-05-23T14:00:00Z] → state=PATROLLING`

---

##  CAG Layer (`src/cag/`)

The **Commander Air Group** is the async orchestration layer that sits between `main.rs` and the Squadron/Orchestrator. It owns the market rotation loop for each asset and manages session-level state.

```
CAG
├── Cag              →  global registry (shared across all asset tasks)
├── SessionState     →  per-asset P&L, starting/live collateral, position tracking
├── RunArgs<P>       →  typed bundle passed into each concurrent market-loop task
└── run_market_loop  →  async fn — the full patrol loop for one asset
```

### Multi-asset concurrency

Set `ASSETS=btc,eth,sol` to run three independent patrol loops in parallel. Each asset gets its own:
- Price Raptor + Funding Raptor (watch channels)
- `SessionState` (isolated P&L and collateral tracking)
- LLM Advisor background task
- `tokio::spawn`ed `run_market_loop` task

**Shared** across all assets: `trading_client`, `nonce_manager`, `wallet_provider`, CAG registry, `DynamicConfig` watch channel, axum API server.

```bash
# .env — multi-asset (BTC + ETH + SOL in parallel)
ASSETS=btc,eth,sol

# .env — single-asset fallback (backward-compatible)
CRYPTO_FILTER=btc
```

> Each asset owns its own SQLite DB file (`logs/btc-dradis.db`, `logs/eth-dradis.db`, etc.). The primary asset (first in `ASSETS`) also backs the default REST API view; pass `?asset=eth` query params to scope API responses to a specific asset pool.

---

## ️ Control Tower — The Dashboard

DRADIS ships with a real-time web dashboard called **Control Tower** built on Next.js 15 + Tailwind CSS.

![Control Tower Dashboard](docs/ui-screenshot.png)

| Panel              | What it shows                                                                                    |
|--------------------|--------------------------------------------------------------------------------------------------|
| **Status Bar**     | Engine online/offline, GHOST mode badge, active market, current BTC price, session P&L           |
| **P&L Chart**      | Rolling equity curve across recent snapshots                                                     |
| **Viper Cards**    | Live enabled/disabled toggle + all parameters editable inline without a restart                  |
| **Open Positions** | In-flight positions with entry time, side (YES/NO/UP/DOWN in correct color), entry price, shares |
| **Telemetry**      | Live Raptor macro cards — including the **Institutional Pulse** card (Tide pulse dial, coherence, per-ETF premium bps; greyed outside US market hours) |
| **Trade Log**      | Last N completed trades with strategy, side, entry/exit prices, shares, P&L, exit reason         |

### Live Config Editing

Every parameter in the Viper cards maps directly to the runtime `DynamicConfig`. Editing a value sends `PATCH /api/config` — **no restart required**. The edit is fanned out to every squadron's config row and reaches running squadrons within ~30 seconds (each squadron reloads its config on a periodic timer).

> **Hot-Enable Design** — All eight Vipers are always instantiated at startup. The `DynamicConfig` enable flags are the sole runtime gate. Toggle any Viper on or off during a live session; the change takes effect on running squadrons within ~30 seconds.

### Authentication

```bash
# .env (production)
CT_USERNAME=starbuck
CT_PASSWORD=your-strong-password
```

---

## LLM Advisor

Optional background task. Every `LLM_ADVISOR_INTERVAL_SECS` (default: 30 min) it fetches recent trades from SQLite, analyzes them with an LLM, and posts plain-English optimization recommendations to Telegram + the Control Tower.

The backend is **provider-neutral** and selected at **runtime** (no rebuild). All provider wiring lives behind a small DRADIS-owned trait in `src/helpers/llm_client.rs` (built on [`rig-core`](https://crates.io/crates/rig-core)); the advisor loop only ever sees a `Box<dyn LlmChat>`. The default is Ollama, so existing deployments that only set `OLLAMA_URL`/`OLLAMA_MODEL` keep working **unchanged** (same endpoints, `num_ctx=3072`/`num_predict=900`/`temperature=0.3`, same timeouts and probe).

### Provider matrix

| `LLM_PROVIDER` | Backend | Auth | Base URL |
|---|---|---|---|
| `ollama` (default) | Local/remote Ollama (`/api/chat`) | none | `LLM_BASE_URL` → `OLLAMA_URL` → default `http://localhost:11434` |
| `anthropic` | Claude models | `ANTHROPIC_API_KEY` | optional override via `LLM_BASE_URL` |
| `openai` | OpenAI platform (Chat Completions) | `OPENAI_API_KEY` | optional override via `LLM_BASE_URL` |
| `openai-compatible` | vLLM / LM Studio / OpenRouter / Groq … | `OPENAI_API_KEY` (optional) | **required** `LLM_BASE_URL` |
| `chatgpt` | "Sign in with ChatGPT" OAuth | subscription token / OAuth file | — |

### Configuration

```rust
// src/config.rs — compile-time defaults
pub const ENABLE_LLM_ADVISOR: bool = true;
pub const LLM_ADVISOR_INTERVAL_SECS: u64 = 1800;
pub const LLM_PROVIDER: &str = "ollama";      // ollama|anthropic|openai|openai-compatible|chatgpt
pub const LLM_MODEL: &str = "";               // "" = ollama uses LLM_OLLAMA_MODEL; cloud requires a value
pub const LLM_BASE_URL: &str = "";            // "" = provider default (ollama falls back to LLM_OLLAMA_URL)
pub const LLM_CLOUD_TIMEOUT_SECS: u64 = 120;  // cloud inference timeout (ollama keeps LLM_INFERENCE_TIMEOUT_SECS=480)
pub const LLM_OLLAMA_URL: &str = "http://localhost:11434";
pub const LLM_OLLAMA_MODEL: &str = "llama3.2";
```

Everything is overridable at runtime via env (`.env`), no rebuild required. See `.env.example` for the full list.

### Per-provider setup

**Ollama (default — unchanged):**
```bash
# ollama pull llama3.2
OLLAMA_URL=http://192.168.1.10:11434   # legacy vars still honoured
OLLAMA_MODEL=mistral
```

**Anthropic (Claude):**
```bash
LLM_PROVIDER=anthropic
LLM_MODEL=claude-3-5-sonnet-latest
ANTHROPIC_API_KEY=sk-ant-...
```

**OpenAI:**
```bash
LLM_PROVIDER=openai
LLM_MODEL=gpt-4o-mini
OPENAI_API_KEY=sk-...
```

**OpenAI-compatible (LM Studio / OpenRouter / vLLM / Groq):** `LLM_BASE_URL` is required — include the `/v1` suffix if your server needs it. Local servers are usually keyless (a dummy key is sent automatically); hosted gateways like OpenRouter need `OPENAI_API_KEY`.
```bash
# LM Studio (keyless, local)
LLM_PROVIDER=openai-compatible
LLM_BASE_URL=http://localhost:1234/v1
LLM_MODEL=qwen2.5-7b-instruct

# OpenRouter (hosted)
LLM_PROVIDER=openai-compatible
LLM_BASE_URL=https://openrouter.ai/api/v1
LLM_MODEL=meta-llama/llama-3.1-8b-instruct
OPENAI_API_KEY=sk-or-...
```

**ChatGPT (OAuth subscription):** authenticates against the "Sign in with ChatGPT" backend. **This bills to a ChatGPT subscription, not to OpenAI API credits.** Simplest for a headless server is an explicit access token; otherwise place a rig-format `auth.json` (auto-refreshing) at `~/.config/chatgpt/auth.json` or point `CHATGPT_AUTH_FILE` at your own file. A missing/expired token with no refresh triggers an interactive device-code login (not suitable for headless).
```bash
LLM_PROVIDER=chatgpt
LLM_MODEL=gpt-5.3-codex
CHATGPT_ACCESS_TOKEN=<oauth-access-token>   # or leave unset to use the OAuth auth.json
# CHATGPT_ACCOUNT_ID=<optional>
# CHATGPT_AUTH_FILE=/path/to/auth.json
```

The stored recommendation is tagged `provider/model` (e.g. `anthropic/claude-3-5-sonnet-latest`) and rendered as the Control Tower badge.

---

## Backtesting (`backtest` feature)

An offline harness that replays historical **Hyperliquid** 1-minute candles + funding
through the **real viper strategies** (the same `Strategy` objects the live bot runs),
behind the clock seam so warmup / staleness / cooldown / hold-time gates evaluate
against *historical* time at any replay speed. It is behind a **non-default** cargo
feature so normal builds/CI never pay its cost.

```bash
# System deps (rs-backtester drags a plotting/font stack):
sudo apt-get install -y libfontconfig1-dev pkg-config

# Replay the last ~5h of BTC through every viper:
cargo run --features backtest --bin backtest -- \
  --coin BTC --start now-6h --end now-1h

# Sweep a subset with a custom book model:
cargo run --features backtest --bin backtest -- \
  --coin BTC --start 2026-07-01T00:00:00Z --end 2026-07-01T06:00:00Z \
  --strategies momentum,trendreversal --spread 0.02 --depth 500 --commission 0.0 \
  --out backtest_out --cache backtest_cache.sqlite
```

Each run prints a per-strategy table (trades / win-rate / native PnL), the
rs-backtester Sharpe/drawdown/win-rate, and a **fidelity-tier disclaimer**, and writes
`report.json` + `trades.csv` + `equity.csv` into `--out`.

**Two PnL views, deliberately:**
1. **Native Decimal ledger** (authoritative) — prices the actual binary YES/NO shares
   and settles 0/1 at expiry vs strike.
2. **rs-backtester metrics** (directional proxy) — `BUY/SHORTSELL/NULL` on the
   underlying candle series; a linear-instrument proxy, **not** the binary payoff.

**Honest fidelity tiers** (printed with every result):
- **Tier A** — `drift_10m`/`drift_60m` gates and strike-distance gates are faithful.
- **Tier B** — velocity gates: `velocity_5s`/`velocity_1s` come from 1m closes at 60s
  steps, so sub-5s windows are ~0 (velocity-gated logic is approximate). Funding is a
  real historical series (HL hourly rate **×8**-normalized onto Binance's per-8h scale;
  funding is a **signal only** — binary shares pay no carry).
- **Tier C** — the 8 Polymarket book fields are a parametric binary-option model
  (`--spread`/`--depth`), **not** a real order book; OI/CVD and
  `institutional_pulse`/`tide_coherence` have no historical source (= 0), so
  **Convergence is excluded** (always no-ops) and **TrendReversal's SQLite cascade
  guard no-ops**.

The `--cache` SQLite file is separate from the live trading DB and fills gaps only
(re-runs are free). `--llm-score` (experimental, off by default) asks the configured
LLM provider for a 0–100 conviction score per entry and joins it against realized PnL
in `report.json`; it degrades gracefully if no provider is configured, and API keys
are never logged. `hyperliquid-backtest` is deliberately **not** a dependency — its
advertised fetch/backtest/report API is unreachable dead code in the published crate.

> ⚠️ On Linux, never call rs-backtester's `plot()`/`i_chart()` — they hard-code a
> Windows path join. The harness only reads `Backtest.metrics` + `report_horizontal`.

---

## ️ Safety Systems

- **Circuit breaker**: Pauses all trading after 3 consecutive execution failures.
- **TOCTOU-safe entry**: Atomic lock scope prevents duplicate orders.
- **Orphaned pair detection**: Arbiter waits 5s after first-leg confirm before acting on a missing second leg. TimeDecay GTC bids are given the full theta window (up to 30 min) before a resting order is declared orphaned.
- **Rescue-profit gate**: Arbitrage entries are blocked when a single-leg failure cannot be rescued into profit (`yes_rescue_cost` or `no_rescue_cost ≥ $1.00` including fees and rehedge buffer).
- **Fee Gates**: Blocks Taker Vipers from entering high-fee (10%+) markets.
- **Chain-sync**: Startup and periodic reconciliation against on-chain wallet state — stale DB rows purged, missing positions re-adopted with correct side labels.

---

## ⚠️ Read This First

**This is experimental software. You will probably lose money. Start in GHOST mode and tune.**

- **Risk**: Momentum trades are directional and can get whiplashed. Arbitrage spreads are thin. None of this is guaranteed profit.
- **US Citizens**: Polymarket is rolling out US access under CFTC regulation via a waitlist.
- **Competition**: Polymarket is full of well-funded, low-latency bots. This project is a learning exercise, not an edge.

---

## Setup

### Requirements
- Rust 1.95+ (or Docker)
- A Polygon wallet with USDC and MATIC
- **A paid Polygon RPC endpoint** (required for auto-settlement)
- Telegram bot token (optional)
- Alpaca API key/secret (optional — free tier; only needed for the **Tide Raptor**'s live IEX ETF feed. Without it the Institutional Pulse card stays idle.)

### Tide Raptor (Institutional Pulse) — optional

The Tide Raptor streams real-time spot-BTC-ETF (IBIT/FBTC/ARKB) prints from Alpaca's
free-tier IEX feed and compares them to a synthetic iNAV (btc-per-share × Binance
oracle) to produce the **Institutional Pulse** and **coherence** signals. It is
BTC-only and active during US market hours (09:30–16:00 ET). To enable it, add your
Alpaca keys to `.env`:

```bash
ALPACA_API_KEY_ID=your-key-id
ALPACA_API_SECRET_KEY=your-secret-key
```

These feed the GBoost feature vector, the Basis tide veto, and the **Convergence**
Viper. Omit them and those consumers simply treat the pulse as neutral/zero.

### RPC Configuration

> ⚠️ **Helius is Solana-only — do not use it for DRADIS.**

Recommended: [Alchemy](https://www.alchemy.com/), [QuickNode](https://www.quicknode.com/), [Infura](https://infura.io/)

```bash
POLYGON_RPC_URL=https://polygon-mainnet.g.alchemy.com/v2/YOUR_API_KEY
```

### Configuration Profiles

`src/config.rs` is gitignored. Copy one of the provided examples before building:

| Profile      | File                                 | Wallet    | Risk   | Vipers            |
|--------------|--------------------------------------|-----------|--------|-------------------|
| Conservative | `src/config.conservative.rs.example` | < $100    | Low    | Maker, Time Decay |
| Balanced     | `src/config.balanced.rs.example`     | $100–$300 | Medium | All eight         |
| Aggressive   | `src/config.aggressive.rs.example`   | $200+     | High   | All eight         |

```bash
cp src/config.balanced.rs.example src/config.rs
cargo build --release
```

---

## Running

### Local Development

```bash
cp .env.example .env
cp src/config.balanced.rs.example src/config.rs

# Intl CLOB (default) — BTC
./start-local.sh

# Intl CLOB — specific asset
./start-local.sh eth

# US Retail venue
VENUE=us ./start-local.sh

tail -f logs/dradis-local.log
./stop-local.sh
```

#### Multi-asset mode

```bash
# .env — run BTC, ETH, and SOL loops concurrently
ASSETS=btc,eth,sol

# Each asset gets its own SQLite DB file:
#   logs/btc-dradis.db  (primary — default REST API / Control Tower view)
#   logs/eth-dradis.db
#   logs/sol-dradis.db
# Use ?asset=eth on API endpoints to scope responses to a specific asset.
```

Log filtering:
```bash
tail -f logs/dradis-local.log | grep -i "trade\|entry\|exit"   # trades
tail -f logs/dradis-local.log | grep "Squadron"                  # deployment lifecycle
tail -f logs/dradis-local.log | grep "btc\|eth\|sol"             # per-asset activity
tail -f logs/dradis-local.log | grep -E "WARN|ERROR"             # problems
```

### Production (Docker)

```bash
./deploy-multi.sh
```

Dashboard: `http://YOUR_SERVER_IP:3002`  
API health: `http://YOUR_SERVER_IP:9000/api/health`

---

## ️ Roadmap

### Recently shipped

- **TrendCapture & TimeDecay tuning** — Three targeted fixes from live production data:
  - Removed `* 1.5` effective-SL multiplier in TrendCapture (SL was inflated 12% → 18%); now uses `trendcapture_stop_loss_pct` directly.
  - `trendcapture_max_entry_price` lowered from `0.72 → 0.55` — avoids late-cycle entries where there is almost no room to run to the 20% TP.
  - TimeDecay arb-wait deadline aligned to `TIME_DECAY_MAX_SECS_TO_EXPIRY` (1800s) so valid resting GTC bids are no longer declared orphaned at 185s.
- **Rescue-profit gate** — Arbitrage entries are now blocked when a single-leg failure cannot be recovered into profit. The gate checks `safe_yes_bid + no_ask + fee + buffer ≥ $1.00` and `safe_no_bid + yes_ask + fee + buffer ≥ $1.00` before the collateral check, preventing entries into markets where a rescue trade would guarantee a loss.
- **Shared OrderLifecycle (Slice 3)** — Venue-neutral position reconciler wired end-to-end for the Intl CLOB venue:
  - `OrderLifecycle::reconcile()` polls `Execution::positions()` + `open_orders()` every 30s and flattens truly stale positions, replacing bespoke per-venue polling.
  - `IntlClobVenue` now fully implements `cancel()`, `positions()`, `open_orders()`, and an `active_tokens` registry (cleared on rotation, populated at arb entry time).
  - `LifecycleConfig::intl()` preset — 30-min stale-order backstop, `flatten_sell_limit: $0.01`.
  - `spawn_lifecycle_task()` in `patrol_tasks.rs` wires the reconcile loop to the peripheral cancel token; `lifecycle.track()` fires on every arb entry success.
  - First-leg confirm grace reduced 30s → 5s: once one arb leg confirms, the missing leg has only 5s as a free maker before the arbiter acts.
  - Token sovereignty cooldown now fires at both rejection sites in `patrol_impl.rs`, eliminating a 7,000+/hr spin-loop.
- **US Retail venue (MVP)** — optional `us_retail` build target for the CFTC-regulated Polymarket US exchange; runs the arbitrage strategy with engine-atomic batched orders and live dashboard support.
- **Phase 3f-7 — Per-asset SQLite DB pools** — Each asset in the fleet now owns its own SQLite file (`logs/btc-dradis.db`, `logs/eth-dradis.db`, etc.):
  - `db::init_for_asset()` / `db::pool_for()` / `db::pool_for_opt()` replace the single global pool
  - All hot-path writes (`record_open_position`, `close_open_position`, `record_trade_db`, etc.) scoped to the correct per-asset pool via `pool_for(&asset_lc)`
  - `sync_open_positions_with_chain` and `purge_settled_legs` iterate ALL registered pools — secondary-asset DBs are fully reconciled on startup and after settlement
  - API endpoints accept `?asset=` query param to scope trades, positions, P&L, and recommendations to any active asset pool
- **Phase 3f-6 — CAG task ownership** — Per-asset `AssetTask { AbortHandle, CancellationToken }` registered in `CagInner`:
  - `register_loop_task()`, `stand_down_asset()`, `loop_asset_names()` wired end-to-end
  - `stand_down_all()` cancels + aborts every running asset loop
  - `RunArgs.cancel` checked at the top of every `'market_loop` iteration
  - `Cag::run()` stub deleted; `src/cag/mod.rs` carries accurate architecture docs
- **OBI Swing Block gate** — `MOMENTUM_OBI_SWING_BLOCK` config constant now wired into all 6 Momentum entry paths (primary, strike-crossing, and no-strike for both bull and bear). Previously computed but never applied.
- **Phase 3 — CAG (Commander Air Group)** — Async dispatch layer replacing the manual market-rotation loop:
  - `src/cag/` — `Cag`, `SessionState`, `RunArgs<P>`, `run_market_loop()`
  - `main.rs` reduced from ~730 lines to ~415 lines; full market loop lives in `cag/run.rs`
  - **Multi-asset**: `ASSETS=btc,eth,sol` spawns one concurrent patrol loop per asset (independent raptors, session state, LLM advisor, SQLite DB)
  - Tokio runtime bumped to 8 worker threads; OS-thread watchdog added (5-minute silence → `process::exit(1)`)
  - Backward-compatible: `CRYPTO_FILTER=btc` (single-asset) still works unchanged
- **Raptor / Viper / Squadron architecture** — Three-layer BSG tactical separation of concerns:
  - `src/raptors/` — Price Raptor (Binance WS) + Funding Raptor (Binance FAPI)
  - `src/vipers/` — eight Viper trading strategies (Momentum, Maker, Arbitrage, Time Decay, Basis, GBoost, TrendCapture, Convergence)
  - `src/squadron/` — `Squadron`, `SquadronRaptors`, `SquadronConfig`, `SquadronState`
  - Each market rotation logs `️ Squadron [...] → state=PATROLLING`
- **Open Positions improvements** — Side column colors YES/UP green and NO/DOWN red; chain-adopted positions show `⛓ adopted`; `chain_adopted` DB column with live migration
- **Side label fix** — `adopt_chain_position` correctly binds the Polymarket outcome string (was storing literal `?`)
- **Viper hot-enable** — All Vipers always instantiated at startup; toggle any live from Control Tower with no restart

### Next up
- US Retail venue hardening — live private fills WebSocket; US re-hedge on single-leg failure
- Kalshi venue integration (venue abstraction layer is ready; community PRs welcome)

### Medium-term
- Static deployment profiles (`profiles.toml`) with per-profile P&L tracking
- Squadron creator in Control Tower
- LLM live config patches via Telegram approval gate

### Longer-term
- Sports Raptor (line movement feeds)
- Politics Raptor (polling aggregator feeds)

---

## Integrations

### OpenClaw (Natural-Language Control)

```bash
openclaw skills install dradis-tactical-command
```

| You say                            | Effect                              |
|------------------------------------|-------------------------------------|
| *"Pause GBoost"*                   | Stops GBoost entries on next tick   |
| *"Enable ghost mode"*              | Switches to paper trading instantly |
| *"What's my P&L today?"*           | Returns session profit/loss         |
| *"Show open positions"*            | Lists all in-flight positions       |
| *"Tighten GBoost stop loss to 8%"* | Updates risk parameter live         |

```bash
# .env — enables API key enforcement for OpenClaw
DRADIS_API_KEY=replace-with-a-strong-random-secret
```

---

## FAQ

**Why Rust?** Fearless concurrency — evaluating eight Vipers every 50ms needs a multi-threaded runtime with no GIL or GC pauses.

**Can I trade multiple assets at once?** Yes — set `ASSETS=btc,eth,sol` in `.env`. Each asset runs its own independent patrol loop (raptors, session state, LLM advisor, SQLite DB) inside a `tokio::spawn`ed task. The wallet, CLOB client, and API server are shared. Each asset writes to its own DB file (`logs/btc-dradis.db`, `logs/eth-dradis.db`, etc.); pass `?asset=eth` to any API endpoint to scope results to that asset.

**Why isn't the bot trading?** Check: (1) `GHOST_MODE` true? (2) High-fee market? (3) Thresholds too tight in `config.rs`? (4) No Window/Daily market for Maker/Arb/Basis?

**I see two Vipers on the same token — is that a bug?** No. Each Viper has its own independent position book.

**How do I adjust risk live?** Use the Control Tower Viper cards or `PATCH /api/config`. No restart needed.

**GBoost producing garbage after an update?** The model file is incompatible across feature vector changes. Delete old files and let it cold-start:
```bash
rm -f logs/gboost_model_*.json
```
The safe pattern: bump the suffix in `GBOOST_MODEL_PATH` (e.g. `v14f` → `v15f`) when adding a new feature in `src/vipers/gboost_impl.rs`.

**Can I enable a Viper mid-session?** Yes — all eight are always instantiated. Toggle via Control Tower or `PATCH /api/config`. Takes effect on the next 50ms tick.

**Does DRADIS support the US Polymarket API?** Yes.  Polymarket's **US platform** is a separate, custodial, CFTC-regulated exchange with web2 auth (API key / secret / session token) and string/UUID market IDs. We have **venue abstraction** so a build can target either market via a Cargo feature flag (`intl_clob` default, `us_retail` available) — single-venue per binary, so the US deployment carries none of the Polygon crypto weight and stays inside its own regulatory/network footprint. Start a US build with `VENUE=us ./start-local.sh`.

**What about Kalshi?** Not yet implemented, but the venue abstraction layer (`Execution` trait + `OrderLifecycle`) is complete, so adding Kalshi is a matter of implementing one trait. We will review PRs from the community if offered.

**Control Tower shows "Offline"?** Check: (1) DRADIS running? (2) `curl http://localhost:9000/api/health`? (3) Docker — same `dradis-net` network?

**How can I tune my instance for maximum performance?** Please see our dedicated performance tuning guide: [PERFORMANCE_TUNING.md](docs/PERFORMANCE_TUNING.md).

**How do I enable the LLM Advisor?**
1. `ollama pull llama3.2`
2. `ENABLE_LLM_ADVISOR = true` in `config.rs`
3. `cargo build --release`
4. Set `TELEGRAM_BOT_TOKEN` + `TELEGRAM_CHAT_ID` in `.env`

**Why doesn't DRADIS include a backtesting framework?**

| Concern              | Backtester                                                | Ghost (Paper) Mode                          |
|----------------------|-----------------------------------------------------------|---------------------------------------------|
| Market data fidelity | Requires storing full L2 orderbook snapshots              | Real-time Polymarket CLOB — 100% authentic  |
| Strategy fidelity    | Must mock async execution, cooldown maps, drawdown guards | Full production code path runs unchanged    |
| Fill simulation      | Assumes fills that may never occur in thin markets        | Depth-aware simulated fills (see below)     |
| Build/maintain cost  | Significant                                               | Zero — flip the runtime GHOST/LIVE toggle   |

Workflow: ghost overnight → `tools/session_parser.py` → tune `config.rs` → repeat until positive expectancy.

**Paper trading (Ghost Mode)**

The Control Tower GHOST/LIVE toggle is a real runtime switch, not just a badge. In the
default `intl_clob` build it controls order placement live via the hot-patchable
`DynamicConfig` — no rebuild required. The compiled `config::GHOST_MODE` constant now
only *seeds* the initial toggle state (and is recorded in the startup config-history
snapshot); it no longer gates trading.

- **Position-scoped, grandfathered.** Each position permanently carries the mode it was
  opened under. Flipping the toggle affects only NEW orders (within ~30 seconds on a
  running squadron): open positions keep the mode they were opened with. Exits, orphan
  sweeps, and expiry settlement all key off the position's own mode, never the current toggle.
- **Depth-aware simulated fills.** Ghost entries fill up to the visible ask depth at the
  requested price (± the usual offsets); any overflow fills one tranche worse by
  `PAPER_OVERFLOW_SLIPPAGE` (default 2¢), weighted-averaged into `avg_entry`. Ghost exits
  are symmetric against the bid. No more instant, full, frictionless fills.
- **Resting maker quotes.** Ghost `MakerQuote`s no longer instafill. They rest in a
  simulated queue and fill only after the market's best bid has sat at/below the quote
  for `PAPER_MAKER_FILL_TICKS` (default 3) consecutive patrol ticks.
- **Paper ledger.** A simulated collateral ledger is seeded from
  `PAPER_STARTING_COLLATERAL` (default $1000). Ghost entries debit their cost and are
  rejected (warn + skip) when the ledger is insufficient; ghost exits and settlements
  credit their proceeds. Paper P&L and paper balance are tracked **separately** from live
  money and surfaced on `GET /api/status` (`paper_pnl`, `paper_balance`) and in the
  Control Tower's "Paper Ledger" card. As a result, `total_pnl` (the headline session
  P&L) is now **live-only** — ghost activity no longer commingles into it.
- **Binary expiry settlement.** Ghost positions that reach expiry without an explicit
  exit now settle at their binary payout ($1 if the market resolved in the token's
  favour vs. the strike, else $0) instead of silently vanishing — booking realized paper
  P&L and recording a `expiry_settlement_sim` trade.
- **Full DB parity.** Ghost exits and maker fills write the same rows live ones do
  (`trades` + `open_positions`), and both the `trades` and `entries` tables now carry a
  `ghost_mode` column, so paper and live history are cleanly separable across sessions.
  The Control Tower badges ghost trades/positions with 👻 and labels ghost
  notifications `👻 [PAPER]`.

Not in scope (deliberately): credential-free boot — the default build still requires
`POLYGON_RPC_URL` + a Polymarket-accepted key + live CLOB auth even in ghost mode.
