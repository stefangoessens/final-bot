# Actionable Fixes & Refactoring Plan (Prioritized)

**Severity: CRITICAL**

This is a concrete, implementation-oriented checklist. Items are ordered by “prevents catastrophic failure” first, then “improves edge / competitiveness”.

## P0 — Must fix before any live money (CRITICAL)

1) **Fix RTDS Chainlink subscribe payload**
   - File: `src/clients/rtds_ws.rs`
   - Make Chainlink `filters` a JSON object (not a string).
   - Add a startup watchdog: if no Chainlink update within `chainlink_stale_ms`, stay paused and emit a clear error.

2) **Make StateManager non-blocking on quote publication**
   - File: `src/state/state_manager.rs`
   - Replace `.send(...).await` with coalescing:
     - `watch::Sender<QuoteTick>` per slug, or
     - bounded `mpsc` with `try_send` + drop old ticks.
   - Requirement: feed processing must never block on strategy speed.

3) **Harden order posting against partial batch failures**
   - File: `src/clients/clob_rest.rs`
   - Change `post_orders()` to return successes + failures (do not discard successful order IDs).
   - File: `src/execution/order_manager.rs`
   - Update cache for successes even when some orders fail; retry failed ones next tick.
   - Add test: “partial batch failure does not orphan orders”.

4) **Allow cancellations during REST backoff**
   - File: `src/execution/order_manager.rs`
   - Split execution into:
     - cancels (always attempt),
     - posts (backoff applies).
   - Add test: “quoting disabled triggers cancels even under post-backoff”.

5) **Enforce expiry cutoff in execution (hard gate)**
   - Files: `src/execution/order_manager.rs`, `src/state/state_manager.rs`
   - Requirement: after `cutoff_ts_ms`, *no new placements* can occur even if strategy emits quotes; cancels must still be attempted.
   - Add test: “no post after cutoff; cancel still runs”.

6) **Gate quoting on user WS health and market tradability**
   - Files: `src/state/state_manager.rs`, `src/ops/health.rs`, `src/market_discovery.rs`
   - Add hard prerequisites for quoting when not dry-run:
     - user WS healthy (within `user_ws_stale_ms`),
     - market flags: `active && accepting_orders && !closed && !restricted`.
   - If `restricted`: cancel all + halt.

7) **Wire taker completion or explicitly disable it**
   - Files: `src/strategy/risk.rs`, `src/strategy/engine.rs`, `src/clients/clob_rest.rs`, `src/execution/*`
   - Either:
     - implement completion command path (FAK/FOK with `p_max`), or
     - remove/disable config fields to avoid a false sense of safety.
   - Add test harness: simulate time-to-cancel entering taker window and validate completion trigger behavior.

8) **Fix market end / redeem lifecycle**
   - Files: `src/market_discovery.rs`, `src/state/state_manager.rs`, `src/inventory/engine.rs`
   - Do not drop ended markets until:
     - all open orders canceled, and
     - merge/redeem complete (inventory cleared), or
     - you persist the necessary condition_ids into a separate “post-close” tracker.
   - Prefer scheduling redeem based on `interval_end_ts` + onchain readiness over Gamma `closed` alone.

9) **Add maker-only compliance integration test**
   - Goal: prove “never taker” outside explicit completion flows.
   - Pass: 0 taker fills; post-only rejects are handled without churn loops; order ops stay within budget.

10) **Add deterministic execution race simulator**
   - Build a `SimExchange` / replay-driven harness that can reorder/delay:
     - HTTP acks/rejects/timeouts/429,
     - user WS order updates,
     - user WS trades,
     - disconnect/reconnect sequences.
   - Pass invariants: no negative remaining; no ghost orders; inventory == fills; restart does not duplicate orders.

## P1 — High-value safety/edge improvements (HIGH)

1) **Parse and handle rejected orders from user WS**
   - File: `src/clients/clob_ws_user.rs`
   - Extend parser for rejection types/statuses; carry reason fields.
   - File: `src/execution/order_manager.rs`
   - Remove/update cache entries on rejection events.

2) **Make market WS staleness correct and per-token**
   - Files: `src/alpha/toxicity.rs`, `src/ops/health.rs`, `src/state/market_state.rs`
   - Add explicit `market_ws_stale_ms` config.
   - Treat a token stale if `now_ms - token.last_update_ms > market_ws_stale_ms`.
   - Decide policy: cancel stale token orders only vs pause entire market.

3) **Make tick-size readiness explicit**
   - Files: `src/state/market_state.rs`, `src/strategy/quote_engine.rs`
   - Initialize tick sizes to 0 and do not quote until tick sizes are confirmed.

4) **Unify order state ownership (fix rewards scoring path)**
   - Option A (spec-aligned): StateManager owns order state; OrderManager feeds updates via AppEvents.
   - Option B (pragmatic): RewardEngine subscribes to OrderManager’s cache snapshot.

5) **Add restart reconciliation**
   - Files: `src/main.rs`, `src/clients/clob_rest.rs`, `src/state/*`
   - On startup:
      - fetch active orders and cancel/sync deterministically,
      - fetch positions/inventory so skew/merge logic starts from truth.
   - UNKNOWN (doc check): use Polymarket-supported idempotency keys / client order ids if available; otherwise persist an intent→order mapping for safe retries.

6) **Move price/size representation to integer units**
   - Goal: eliminate tick-boundary and float formatting edge cases.
   - Use `price_ticks` and `size_base_units` internally; convert to `Decimal` only at the signing boundary.

7) **Add explicit order-ops budget + rate limiting**
   - Implement token bucket / ops budget per endpoint group; handle 429 with jittered backoff.
   - Enforce “do not churn” under budget pressure by widening/reducing updates rather than retrying.

8) **Address self-churn risk from best_bid-based “improve”**
   - If `base_improve_ticks > 0`, add safeguards to avoid outbidding yourself (see `02_STRATEGY_SOUNDNESS.md`).

## P2 — Performance + operability (MEDIUM)

1) Reduce `info` logs in hot loops; add sampling and “only log on change”.
2) Move event-log file flushes to `spawn_blocking` or a dedicated thread.
3) Add real rate limiting (token bucket) per endpoint group.
4) Consolidate hyper versions (avoid `hyper 0.14` + hyper 1 duplication).
5) Make raw frame logging configurable/sampled per feed (market/RTDS/user).
6) (Optional) Refactor signing to avoid SDK global mutable fee state if the SDK supports it; keep current mutex until proven safe.
7) Add `RUNBOOK.md` (drills: user WS stale, Chainlink stale, cutoff, restart) and document required config knobs + alerting.

## Validation checklist (concrete)

- RTDS: receive both Binance + Chainlink updates within threshold; bot transitions from paused → quoting enabled.
- Order posting: partial failures do not orphan orders; cache always matches user WS.
- Kill switches: user WS stale, Chainlink stale, cutoff, and shutdown all result in confirmed cancel-all behavior.
- Rollover: ended market positions are merged/redeemed; no stranded inventory after a boundary.

## Definition of Done (production candidate)

- [ ] Maker-only compliance test passes (0 taker fills outside explicit completion)
- [ ] Deterministic race simulator passes invariants (no negative remaining, no ghost orders)
- [ ] Restart reconciliation test passes (no duplicates, cache matches exchange)
- [ ] Cutoff gate proven at execution layer (no posts after cutoff, cancels still attempted)
- [ ] Stale user WS / stale Chainlink drills cancel+pause reliably
- [ ] Order ops budget + 429 backoff verified in paper runs
