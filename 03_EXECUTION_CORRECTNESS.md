# Execution Correctness — Order Lifecycle, Races, Idempotency

**Severity: CRITICAL**

This section reviews the order lifecycle implementation against a minimal MM state machine (Place → Ack → Live → Partial Fill → Cancel/Replace) and evaluates race conditions and restart behavior.

## Scope note

Execution correctness is mostly about **invariants under adversarial ordering** (out-of-order WS vs HTTP, disconnect/reconnect, partial failures). If you don’t have deterministic tests for those, you are shipping unknown risk.

## Observed architecture

- **StrategyEngine** emits `ExecCommand { slug, desired: Vec<DesiredOrder> }` every quote tick (`src/strategy/engine.rs`).
- **OrderManager** diffs desired vs its internal cache (`MarketOrderCache`) and executes cancel → post batches (`src/execution/order_manager.rs`).
- **User WS** sends:
  - trade fills to StateManager (inventory),
  - order updates to OrderManager (`src/clients/clob_ws_user.rs`).
- **StateManager** does not track live orders (its `MarketState.orders` is never populated outside tests).

This split can work, but only if OrderManager is robust to partial failures, replays, and restart reconciliation. Right now it is not.

## CRITICAL issues

### 0) (Positive) FeeRateBps signing is mutex-protected, which avoids a known class of rejects

- Evidence: `src/clients/clob_rest.rs` fetches feeRate per token and does `set_fee_rate_bps(token_id, ...)` under `sign_mutex` immediately before signing.
- Why it matters: if the underlying SDK stores feeRateBps in mutable client state, concurrent signing can produce wrong signed payloads.
- Remaining risk:
  - Throughput: serializes signing (likely OK given rate limits).
  - Staleness/failure: if fee-rate fetch fails, current error paths can cascade into OrderManager backoff (and currently can block cancels).

### 1) Batch order posting can orphan live orders (partial success handling)

- Evidence: `src/clients/clob_rest.rs::post_orders()`
  - Collects success `order_id`s, but returns `Err` if any failure occurs, discarding the success ids.
- Impact:
  - The exchange may have accepted some orders, while the bot treats the whole batch as failed.
  - OrderManager then does not store those accepted `order_id`s → cannot cancel/replace them → may double-post.
- Action:
  - Return a structured result: `{ accepted: Vec<OrderId>, rejected: Vec<{idx, reason, maybe_order_id}> }`.
  - Update cache for accepted orders regardless of rejects.
- UNKNOWN validation:
  - Determine batch atomicity by experiment (see executive summary).

### 2) REST backoff suppresses cancellations (unsafe under kill-switch scenarios)

- Evidence: `src/execution/order_manager.rs::handle_command_with_logger()`
  - If `backoff_until_ms > now_ms`, the method returns early (no cancels, no posts).
- Impact:
  - In stale-oracle / cutoff / shutdown conditions, you may fail to cancel orders.
- Action:
  - Always execute cancels; only back off posts.
  - Consider escalation path: if repeated cancel errors, call `cancel_all_orders` once.

### 3) User WS order parsing does not handle “rejected” updates

- Evidence: `src/clients/clob_ws_user.rs::parse_order_event()`
  - Accepts only `PLACEMENT | UPDATE | CANCELLATION` (plus fallback for `event_type == order`).
- Impact:
  - If Polymarket emits explicit rejection events (common in CLOBs for post-only or tick violations), OrderManager will never remove/update those orders.
  - Cache drift → stale quotes, inability to replenish, and unexpected exposure.
- Action:
  - Extend parser to handle rejection status/types from official schema.
  - Ensure OrderManager removes rejected orders immediately.
- Experiment:
  - Force a deterministic post-only reject and capture the raw user WS frame; add a regression test with that frame.

### 4) Restart idempotency is not guaranteed (missing reconciliation)

- Evidence:
  - On startup, the bot does a best-effort `cancel_all_orders()` but continues on failure (`src/main.rs`).
  - There is **no** initial sync of:
    - active orders,
    - positions/inventory,
    - outstanding fills since last run.
- Impact:
  - If startup cancel fails (auth issue, transient error), bot can post duplicates and lose track of pre-existing orders.
  - Inventory state resets to zero after restart → skew logic and merge/redeem can operate on false assumptions.
- Action:
  - Make startup deterministic:
    - If not dry-run: require successful cancel-all (or fetch+cancel per market) **or halt**.
    - Fetch active orders and seed OrderManager cache.
    - Fetch positions/inventory and seed MarketState inventory (or onchain balances).
  - UNKNOWN (requires doc check): if Polymarket supports a client-provided idempotency key / client order id, use it for all placements; otherwise persist an “intent_id → order_id” mapping and reconcile on restart.

## HIGH issues

### 1) Order scoring integration cannot work with the current ownership of order state

- Evidence: RewardEngine reads `MarketState.orders.live`, but OrderManager is the only component tracking orders.
- Impact: liquidity rewards chasing is non-functional.
- Action: decide a single source of truth:
  - Option A: StateManager owns `MarketState.orders` and OrderManager reports updates via events.
  - Option B: RewardEngine subscribes directly to OrderManager’s live order IDs via a `watch`/channel.

### 2) Diff logic depends on inferred tick size (can disable repricing)

- Evidence: `OrderManager` maintains `tick_size` cache inferred from desired ladder prices (`update_tick_cache`).
- Impact:
  - If tick size is unknown/incorrect, `needs_update()` may not reprice (price_update disabled when tick=0).
  - Leads to stale quotes in edge conditions (e.g., if level spacing collapses due to caps).
- Action:
  - Include tick size explicitly in `ExecCommand` (or maintain a shared tick cache fed from Market WS).

## Races and ordering (observed + recommended mitigation)

1) **Fill vs order-update arrival**
   - Observed: fills update inventory (StateManager), order updates update remaining size (OrderManager). Arrival order is not guaranteed.
   - Mitigation: treat both as independent signals; never rely on one for correctness of the other. This is mostly OK today.

2) **Duplicate events after reconnect**
   - Observed: no dedupe for fills (no trade_id tracked).
   - UNKNOWN: whether Polymarket user WS can replay fills on reconnect.
   - Experiment: disconnect/reconnect user WS and observe if recent trades are replayed.
   - If replay occurs: implement dedupe keyed by a unique trade identifier from the schema (required).

## Required test harnesses (non-negotiable for “production candidate”)

1) **Deterministic execution race simulator**
   - Build a `SimExchange` (or adapter around recorded logs) that can emit:
     - HTTP responses (acks, rejects, timeouts, 429),
     - WS order updates,
     - WS trades/fills,
     - disconnect/reconnect sequences,
     in randomized order with controlled delays.
   - Invariants:
     - remaining size never negative,
     - cancel-after-fill does not resurrect orders,
     - cache state converges to exchange state,
     - no duplicate orders per (token, level) intent across retries/restarts.

2) **Maker-only compliance integration test**
   - Force near-crossing quotes and verify:
     - post-only clamps (or rejects) are handled without churn loops,
     - taker fills remain zero except explicit completion.

## Numeric correctness note (price/size representation)

- Current code uses `f64` through quote generation and later converts to `Decimal` for signing (`src/clients/clob_rest.rs`).
- Action (recommended): represent prices/sizes internally as integer ticks/base units (`i64`) and only convert at the boundary. This makes:
  - tick alignment exact,
  - “near boundary” rounding bugs testable,
  - signatures stable.
