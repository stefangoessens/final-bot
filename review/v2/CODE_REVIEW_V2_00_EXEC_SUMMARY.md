# CODE REVIEW (V2) — Polymarket BTC 15m MM Bot (Rust)
Date: 2026-01-27

This review is a **system design + safety + correctness + performance** audit of the **latest `final-bot-main.zip`** you uploaded.

## Scope & constraints
- I reviewed the **static source** in this ZIP. I did **not** run `cargo test`/`cargo check` here (tooling unavailable in this environment), so anything that depends on compilation/runtime behavior is called out explicitly.
- I checked alignment against:
  - Polymarket docs: `https://docs.polymarket.com/`
  - CLOB/Trading docs: `https://docs.polymarket.com/#clob-api`
  - WebSocket docs: `https://docs.polymarket.com/#websocket-api`
  - Maker rebates & fees docs: `https://docs.polymarket.com/developers/market-makers/maker-rebates-program`

## High-level architecture (as implemented)
Your bot is now a clean multi-task pipeline:

- **Market discovery** (`src/market_discovery.rs`): finds current/next BTC 15m markets and publishes subscriptions.
- **Market WS** (`src/clients/clob_ws_market.rs`): maintains top-of-book + tick size per token.
- **User WS** (`src/clients/clob_ws_user.rs`): streams fills/orders and provides a liveness heartbeat.
- **RTDS WS** (`src/clients/rtds_ws.rs`): ingests Binance + Chainlink BTC/USD (Chainlink = settlement truth).
- **StateManager** (`src/state/state_manager.rs`): fuses all feeds, computes staleness, cutoff, and exports `MarketState` ticks.
- **Alpha** (`src/alpha/*`): computes `q_up`, `target_total`, toxicity regime.
- **Strategy/quotes** (`src/strategy/*`): builds 2-sided ladders, applies inventory skew, applies scoring nudges.
- **OrderManager** (`src/execution/order_manager.rs`): diff → cancel/replace/post, handles user order updates, supports restart modes.
- **Completion** (`src/execution/completion.rs`): last-window taker completion with explicit cap (but see Critical Issue #1 below).
- **Inventory merge/redeem + onchain** (`src/inventory/*`): merges matched sets pre-expiry, redeems post-resolution (EOA mode).
- **Persistence** (`src/persistence/*`): event log with rotation + disk budget.
- **Ops** (`src/ops/*`): `/healthz` + `/metrics`.

This architecture is consistent with an “alpha vs execution” split and is **much closer to production** than the previous snapshot.

## Alignment with your non‑negotiables
✅ **BTC 15m Up/Down only**  
- Market discovery is scoped by `market_slug_prefix` (see `BotConfig::validate` and `MarketDiscoveryLoop`), and the default is `btc-updown-15m`.

✅ **Quotes maker bids on BOTH outcomes (multi-level ladder)**  
- `QuoteEngine::build_orders()` produces ladders for both tokens each tick, subject to gating and inventory skew.

✅ **Stop quoting + cancel orders 60s before market end**  
- `StateManager` computes `cutoff_ts_ms = end - cutoff_s` and stops producing desired orders once past cutoff.

✅ **Settlement truth is Chainlink BTC/USD; Binance is toxicity only**  
- `alpha/toxicity.rs` uses Chainlink as the “primary truth” and Binance only for fast-move regime detection.

✅ **Heartbeats mandatory (cancel-on-disconnect)**  
- Config validation enforces heartbeats outside dry-run; `run_heartbeat_supervisor()` triggers kill-switch behavior.

✅ **Completion only with explicit price cap and FOK/FAK**  
- `strategy/risk.rs` computes `p_max` and `clob_rest.rs` uses `order_type(FOK/FAK).price(p_max)`.
- **BUT** there is a critical bug/ambiguity in how the completion *amount* is expressed (see Critical Issue #1).

## “Agree / fixed?” summary vs my previous review
You fixed most of the items we were aligned on:
- ✅ **Tick size gating**: quotes don’t start until tick size is known; default tick size removed.
- ✅ **Partial-batch post safety**: accepted vs rejected orders are handled without losing accepted IDs.
- ✅ **FeeRateBps concurrency safety**: signing is serialized via a mutex.
- ✅ **Restart safety**: `startup_cancel_all` default true + optional REST seeding mode.
- ✅ **Raw WS logging redaction**: redact + truncate sensitive raw frames.
- ✅ **Merge loop added**: matched set merging is implemented; not waiting for expiry.
- ✅ **Chainlink-vs-Binance “divergence halts” removed**: no longer halts just because Chainlink lags.

Remaining “not fixed / still risky” items:
- ❌ **Completion order uses BUY amount as shares** (should be USDC for market orders) — **critical**.
- ❌ **RTDS keepalive PING not sent** — may cause periodic disconnects.
- ⚠️ **Expected post-only rejects are still treated as hard failures → backoff**.

See **`CODE_REVIEW_V2_01_DELTA_CHECKLIST.md`** for the exact per-item mapping.

## The 3 highest-risk issues (loss/downtime)
1) **Completion BUY amount semantics (critical correctness risk)**  
   - Current code builds a BUY market order using `Amount::shares(shares)` in `src/clients/clob_rest.rs::build_completion_order`.
   - Polymarket docs specify for market orders: **BUY amount is in dollars/USDC**, SELL amount is in shares.  
   - If uncorrected, this can cause rejects, wrong sizing, or unintended fills.  
   - Full fix (with code) is in `CODE_REVIEW_V2_02_CRITICAL_FIXES.md`.

2) **RTDS WS keepalive**  
   - RTDS docs specify sending `PING` messages periodically; current `RTDSLoop` only responds to ping frames but never sends text `PING`.
   - Result: intermittent disconnects exactly when the feed is important (fast moves).

3) **Backoff on “expected” post-only rejects**  
   - In fast markets, post-only orders can race and get rejected (“would match”) even when your logic is correct.
   - Treating those as “system failure” triggers backoff and leaves you unquoted at the worst time.

## Does this bot have a realistic chance of success?
**Execution & safety:** with the remaining P0 fixes, yes — the *system* is approaching production-grade for a 2-asset, 15m cadence market maker.

**Edge/alpha:** still uncertain without data. Your alpha is essentially “fair value + caps + toxicity gating + rewards nudges”. That can work **if** (a) spreads/rewards are consistently enough to overcome adverse selection and fees, and (b) you maintain queue position. This cannot be answered definitively without:
- historical spread + fill-rate stats on these 15m markets,
- your actual maker rebate schedule & scoring sensitivity,
- backtests / paper trading results.

See `CODE_REVIEW_V2_06_STRATEGY_ALPHA_EDGE.md` for concrete improvements you can implement without turning this into a research project.

## What to do next
- Implement the **P0** fixes in `CODE_REVIEW_V2_02_CRITICAL_FIXES.md`.
- Then do a **48h paper-trading** run with full logging + replay to validate:
  - completion sizing + fee model,
  - order placement/cancel reliability under volatility,
  - merge/redeem gas + success rate,
  - scoring nudges impact on rewards and adverse selection.


## Direct answers to your questions

### What specific changes are required to make this bot production-ready?
**P0 (must do before any real money):**
- Fix completion BUY market-order amount semantics (BUY must be USDC). See `CODE_REVIEW_V2_02_CRITICAL_FIXES.md`.
- Add RTDS keepalive PING. See `CODE_REVIEW_V2_02_CRITICAL_FIXES.md`.
- Stop triggering order-manager backoff on expected post-only rejects. See `CODE_REVIEW_V2_02_CRITICAL_FIXES.md`.
- Add explicit cancel-all on graceful shutdown. See `CODE_REVIEW_V2_02_CRITICAL_FIXES.md`.

**P1 (strongly recommended):**
- Enforce `max_orders_per_min` (currently unused config). Patch in `CODE_REVIEW_V2_03_EXECUTION_ORDER_LIFECYCLE.md`.
- Add REST/onchain timeouts around critical calls to avoid hung tasks.
- Add an inventory reconciliation circuit breaker.

### What are the highest-risk issues that could cause losses or downtime?
1) Completion market order “amount” semantics (BUY uses USDC) → mis-sizing near expiry.
2) RTDS keepalive missing → oracle feed dropouts → quoting halted unexpectedly.
3) Backoff on expected post-only rejects → you go dark during volatility.

### Does this bot, as written, have a realistic chance of success on Polymarket BTC 15-minute markets?
**If you apply the P0 fixes:** the bot is technically capable of running reliably and safely.  
**The bigger unknown is edge:** without historical backtests/paper trading, it’s unclear whether the strategy + scoring nudges produces positive net PnL after fees/adverse selection. The system is a good foundation; success depends on measured microstructure + reward economics.
