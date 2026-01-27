# Executive Summary — Polymarket BTC 15m MM Bot (Rust) Code Review

**Severity: CRITICAL**

## Scope & evidence boundaries

- Reviewed the local source under `src/` plus repo specs (`FULL_SPEC.md`, `TECH_SPEC.md`, `APPENDIX_ALPHA_UPDATES.md`, etc.).
- **Evidence-first**: Any Polymarket on-the-wire behavior not explicitly validated against official docs or live experiments is labeled **UNKNOWN** with a concrete test.
- This is an execution+systems review; it does **not** claim the strategy has positive edge without live-paper evidence.

## Bottom line

This codebase is **not production-ready** for real-money trading. It is **paper-trade-only** until the CRITICAL items below are fixed and validated with targeted experiments. The most serious issues are **feed subscription correctness (Chainlink RTDS), execution idempotency/order tracking, liveness under backpressure, and missing safety gating (user WS + market tradability flags)**.

## What looks strong (already present)

- **FeeRateBps signing safety**: `ClobRestClient` fetches `fee_rate_bps` per token and serializes `set_fee_rate_bps → sign` via a mutex (correctness > throughput).
- **Maker-only quote intent**: quote engine produces post-only bids, with combined cap enforcement on level 0.
- **Replay/event capture exists** (JSONL event log + replay/stress runner), which is the right foundation for regression tests.

## Top CRITICAL blockers (must fix before any live deployment)

1) **Chainlink RTDS subscription payload is likely wrong → bot may never quote**
   - Evidence: `src/clients/rtds_ws.rs` builds `filters` for Chainlink as a **stringified JSON** rather than an object (`"filters": chainlink_filter` where `chainlink_filter` is `json!(...).to_string()`).
   - Impact: No Chainlink updates → `alpha::update_alpha()` treats oracle as stale → `MarketState.quoting_enabled` stays false → bot never trades (or trades with wrong oracle assumptions if behavior changes).
   - Fix: Make `filters` an actual JSON object for Chainlink (per `FULL_SPEC.md` section 5.3).

2) **StateManager can deadlock/stall the entire system under channel backpressure**
   - Evidence: `src/state/state_manager.rs` awaits `tx_quote.send(...).await` for each market inside the event loop.
   - Impact: If downstream (fanout/strategy) is slow or blocked, StateManager stops processing feed events; WS loops then back up on `tx_events.send(...).await`, causing stale state, delayed cancels, and potentially disconnects.
   - Fix: Make quote publication non-blocking/coalescing (e.g., `try_send` + drop, or `watch` channel per market).

3) **Quoting safety does not gate on (a) user WS health or (b) market tradability flags**
   - Evidence: `MarketState.quoting_enabled` is only `now_ms < cutoff_ts_ms && out.size_scalar > 0.0` in `src/state/state_manager.rs`.
   - Missing gates:
     - User WS stale/disconnected (spec requires halt/pause).
     - Market flags: `restricted`, `accepting_orders`, `active`, `closed` from Gamma (`MarketIdentity`).
   - Impact: Bot can quote while blind to fills/order updates; can also spam invalid markets and miss geoblock/restriction handling.

4) **REST backoff currently prevents cancellations (worst-case: stale orders remain live)**
   - Evidence: `src/execution/order_manager.rs` skips *all* execution (cancels + posts) if `backoff_until_ms > now_ms`.
   - Impact: If REST errors occur near cutoff or during stale-oracle events, the bot may fail to cancel orders when it most needs to.
   - Fix: Allow cancels even during post backoff; keep backoff only for placements (or separate backoffs).

5) **Batch post error handling can orphan live orders (partial success not tracked)**
   - Evidence: `src/clients/clob_rest.rs::post_orders()` returns `Err` if *any* order in the batch fails, discarding successful `order_id`s already collected.
   - Impact: Orders may be live on-exchange but absent from the bot’s cache; leads to duplicates, inability to cancel, and inventory risk.
   - UNKNOWN: Whether Polymarket batch post is atomic. You must validate (see Experiments).
   - Fix: Return successes + failures separately and update caches for successes.

6) **Liquidity rewards scoring integration is functionally disconnected**
   - Evidence: `RewardEngine` reads `state.orders.live` (`src/strategy/reward_engine.rs`), but `MarketState.orders` is never populated outside tests; user WS order updates go to `OrderManager` only.
   - Impact: Scoring checks never run, and “not scoring → improve” logic never activates.
   - Fix: Provide a single source of truth for live orders (either via StateManager updates or a dedicated order-state broadcast from OrderManager).

7) **Taker completion is implemented but never executed**
   - Evidence: `should_taker_complete()` exists (`src/strategy/risk.rs`) and `build_completion_order()` exists (`src/clients/clob_rest.rs`), but there is no wiring from strategy → execution.
   - Impact: Unpaired inventory can persist into the “stop quoting” window, increasing directional exposure into settlement.

8) **End-of-market resolution/redeem flow appears incomplete/broken under current market-tracking**
   - Evidence: `MarketDiscoveryLoop` drops the ended market slug immediately at the next 15m boundary (`src/market_discovery.rs`), and redemption logic in `InventoryEngine` requires `MarketIdentity.closed == true` (`src/inventory/engine.rs`), which is no longer refreshed for dropped markets.
   - Impact: Markets may never be redeemed; positions can remain stuck.
   - Fix: Keep ended markets in a “post-close tracking” set until redemption completes, or base redemption scheduling on end timestamps + onchain readiness rather than Gamma “closed”.

## Production-candidate bar (must-prove tests, not vibes)

These are the minimum tests/experiments that turn “looks OK” into “safe enough for limited capital”:

1) **Maker-only compliance run (integration)**
   - Pass: *0 taker fills* over long paper runs (except explicit completion flows), and post-only rejects are handled without churn loops.

2) **Deterministic execution race simulator**
   - Feed randomized/controlled reordering of HTTP acks, user WS updates, cancels, and fills.
   - Pass: no negative remaining, no ghost orders, inventory == fills, cancel-before-cutoff always wins.

3) **Restart reconciliation test**
   - Kill/restart while orders live.
   - Pass: bot cancels/syncs deterministically and never double-posts or loses track of live orders.

4) **Staleness/kill-switch drills**
   - Stall market WS, stall user WS, stall Chainlink RTDS.
   - Pass: cancel + pause/halt triggers fire reliably and do not auto-resume until state is fresh.

## Overall assessment by domain

- **Strategy (maker-only quoting)**: Solid skeleton (post-only, combined cap, dynamic target_total), but incomplete around end-of-interval inventory neutralization and market tradability gating.
- **Execution correctness**: Not safe due to partial batch failure handling, missing rejection handling, and lack of restart reconciliation.
- **Latency/perf**: Architecturally vulnerable to backpressure stalls; logging/event capture likely too heavy at `info` in hot loops.
- **Risk & safety**: Several spec-mandated kill switches are not wired into quoting/cancel logic.

## “UNKNOWN” items that require explicit validation (evidence-first)

1) **RTDS subscribe schema acceptance**
   - Experiment: connect to RTDS, send current subscribe payload, confirm receipt of `crypto_prices_chainlink` updates for `btc/usd` within 2s.
   - Pass: Chainlink updates parsed and delivered to StateManager; `quoting_enabled` becomes true (assuming market ws healthy).

2) **Batch post atomicity**
   - Experiment: submit a batch with 1 intentionally invalid order + 1 valid order; check if valid order is placed (via `get_active_orders` or user WS).
   - Pass A (atomic): entire batch rejected → current strict error handling less dangerous (but still should surface per-order reasons).
   - Pass B (non-atomic): valid order placed → current code can orphan orders → must fix.

3) **User WS rejection event schema**
   - Experiment: deliberately trigger a post-only reject and capture raw user WS frame; confirm if `type` includes `REJECTION` or similar.
   - Pass: parser recognizes and OrderManager removes/updates cache accordingly.

4) **Market WS message cadence**
   - Experiment: measure time between `best_bid_ask`/`price_change` events during quiet periods; calibrate `market_ws_stale_ms` accordingly.
