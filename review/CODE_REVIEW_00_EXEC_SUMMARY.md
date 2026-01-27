# Code Review — Polymarket BTC 15m Market Maker (Rust) — Executive Summary

This review is based on the **full codebase** from the attached `final-bot-main.zip` (folder: `final-bot-main/`).

## Bottom line

- **Architecture:** Solid for a “single‑venue maker bot” (separate async loops for market WS, user WS, RTDS, strategy, execution, inventory merge/redeem, health, logging).
- **Spec alignment:** Largely aligned with your non‑negotiables:
  - Maker quoting on **both outcomes**, multi‑level ladder (`src/strategy/quote_engine.rs`).
  - Combined cost target ~`0.985` default (`TradingConfig.target_total_base`) and enforced via `enforce_combined_cap` (`quote_engine.rs`).
  - **Stops quoting & cancels** ~60s before end (`StateManager` computes cutoff, `QuoteEngine` returns empty).
  - **Chainlink is treated as oracle of record**; stale Chainlink disables quoting (`alpha/toxicity.rs`, `state/state_manager.rs`).
  - **FOK/FAK completion** exists (`strategy/risk.rs`, `execution/completion.rs`) but **has a critical bug** (details below).
  - **Heartbeats / cancel‑on‑disconnect**: implemented via Heartbeats API supervisor (`clients/clob_rest.rs` + `main.rs`).

- **Production‑readiness:** Close, but **not production‑ready yet** because of a few high‑risk correctness and reliability issues (below). The biggest one is a taker completion order construction bug.

## Highest‑risk issues (losses / downtime)

### 1) Critical: Taker completion order uses wrong “amount” semantics for BUY market orders
- File: `src/clients/clob_rest.rs`
- Function: `build_completion_order`
- Current code builds a **BUY market order** with `Amount::shares(shares)`:
  - That conflicts with Polymarket’s documented semantics for market orders: **BUY uses a dollar amount; SELL uses shares**.
  - See Polymarket “Create Order” docs (market order types) and L1 method definition for market orders.
- Impact:
  - Best case: SDK rejects the request (no completion when needed).
  - Worst case: you hedge the **wrong size**, creating **bigger inventory skew** instead of reducing it.

**Fix:** Build completion as a **marketable limit** (size in shares + explicit `price = P_max` cap) and set `order_type` to FOK/FAK. Patch is in `CODE_REVIEW_01_CRITICAL_FIXES.md`.

### 2) Likely reliability issue: RTDS WS keepalive may be incomplete
- File: `src/clients/rtds_ws.rs`
- RTDS documentation says clients should send periodic `PING` keepalives.
- Current code handles websocket Ping frames, but does **not** send a text `PING` keepalive (and does not handle text `PONG`).
- Impact: intermittent RTDS disconnects → Chainlink/binance staleness → quoting disabled → missed markets.

**Fix:** Add a dedicated ping task (or split read/write and send `PING`). Patch in `CODE_REVIEW_01_CRITICAL_FIXES.md`.

### 3) Inventory correctness: no periodic “authoritative” reconciliation (beyond startup)
- Startup reconciliation uses Data API positions and open orders (`src/reconciliation.rs`) — good.
- After startup, inventory is updated from user WS “trade” events (`src/clients/clob_ws_user.rs` → `StateManager::apply_fill`).
- If user WS drops messages without tripping the stale detector, or parsing misses a trade edge case, inventory can drift.

**Fix (safe):** Add a **desync watchdog** that compares local inventory vs Data API positions and triggers **cancel + shutdown** on mismatch (do not blindly overwrite inventory because Data API can lag). Patch plan in `CODE_REVIEW_01_CRITICAL_FIXES.md`.

### 4) Start‑of‑market “tick size unknown” can cause avoidable rejects/backoff
- Quote engine falls back to a default tick size (`DEFAULT_TICK_SIZE = 0.001`) if tick isn’t known yet.
- This can generate invalid prices until `TickSizeSeed` arrives.
- Impact: order rejects, execution backoff, lost time early in market.

**Fix:** Gate quoting until both tokens have `tick_size > 0` (state manager block reason). Patch in `CODE_REVIEW_01_CRITICAL_FIXES.md`.

## Does this bot have a realistic chance of success?

**Execution & safety:** With the fixes above, execution looks “market‑maker sound” for Polymarket CLOB:
- post‑only ladder quoting, combined price cap enforcement, per‑tick diff/cancel/post,
- startup reconciliation (optional cancel‑all),
- merge/redeem (EOA mode),
- health gating (user ws stale disables quoting),
- scoring nudges.

**Alpha / edge:** The current alpha is a simple GBM‑style probability estimate based on BTC price change and EWMA volatility (`src/alpha/mod.rs`). That is unlikely to be a durable edge by itself in BTC 15m markets where adverse selection and fast‑move regimes dominate. The bot’s likely edge sources are:
- **maker rebates + scoring** (you already integrated `/orders-scoring` in `reward_engine.rs`),
- **capital velocity via merge** (implemented in `inventory/*`).

To be competitive long‑term, you’ll need:
- tighter toxicity filters and/or faster “quote fading”,
- better microstructure features (even just top‑of‑book dynamics) and calibration,
- continuous post‑trade analytics to tune parameters.

So: **yes, it can succeed**, but only if (a) you fix the critical completion bug, (b) you harden reliability, and (c) you iterate strategy parameters based on real fill + PnL logs.

## References (for spec / behavior verification)

(Links are in a code block so you can copy/paste.)

```text
Polymarket CLOB API — Create Order (order types, postOnly restrictions)
https://docs.polymarket.com/#create-order

Polymarket L1 Methods — createMarketOrder semantics (BUY amount dollars, SELL amount shares)
https://docs.polymarket.com/?shell#l1-methods

Polymarket CLOB API — Order Book Summary (POST /books, tick_size field)
https://docs.polymarket.com/#order-book-summary

Polymarket Maker Rebates Program — taker fee formula
https://docs.polymarket.com/#maker-rebates-program
```

Example BTC 15m market (resolution source = Chainlink) for sanity checks:
```text
https://polymarket.com/market/btc-price-higher-or-lower-in-15-minutes
```
