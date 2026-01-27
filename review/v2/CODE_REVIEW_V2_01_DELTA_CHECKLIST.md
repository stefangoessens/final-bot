# CODE REVIEW (V2) — Delta Checklist vs Previous Review

This file maps the **previous review’s key findings** to the **current codebase** and marks each as:
- ✅ **Fixed**
- ⚠️ **Partially fixed**
- ❌ **Not fixed**

---

## P0 / Critical correctness

### 1) Completion BUY amount semantics (market order)
**Previous finding:** BUY market orders use **dollars/USDC amount**, not shares; using `Amount::shares` for `Side::Buy` is likely wrong.

**Current code:** ❌ Not fixed  
- `src/clients/clob_rest.rs::build_completion_order` still does:
  - `market_order(token_id, Side::Buy)`
  - `.amount(Amount::shares(shares))`

**Why it matters:** may reject or size incorrectly; can cause unbounded skew near expiry if completion misfires.

**Fix:** see `CODE_REVIEW_V2_02_CRITICAL_FIXES.md` (includes Rust patch).

---

### 2) RTDS keepalive (send `PING`)
**Previous finding:** RTDS feed requires client `PING` messages periodically.

**Current code:** ❌ Not fixed  
- `src/clients/rtds_ws.rs` responds to WS ping frames but does **not** send `Message::Text("PING")` on an interval.

**Fix:** see `CODE_REVIEW_V2_02_CRITICAL_FIXES.md` (code patch).

---

## P0 / Restart safety & state correctness

### 3) Startup desync safety: “cancel all” or seed open orders
**Previous finding:** restarting with an empty in-memory cache risks desync; safest is cancel-all on startup (or seed open orders).

**Current code:** ✅ Fixed  
- `startup_cancel_all: true` default in config.
- `reconciliation::startup_*` can seed from REST if you disable cancel-all.

Files:
- `src/config.rs`
- `src/main.rs`
- `src/reconciliation.rs`

---

### 4) Partial success in batch posting
**Previous finding:** `post_orders()` treated “any rejection” as an overall failure and lost accepted order IDs → desync risk.

**Current code:** ✅ Fixed  
- Introduced `post_orders_result()` returning `{accepted, rejected}`.
- `OrderManager` records accepted orders even when some reject.

Files:
- `src/clients/clob_rest.rs::{post_orders_result, post_orders_partial}`
- `src/execution/order_manager.rs`

---

### 5) Tick size: do not assume `0.001`, gate quoting until known
**Previous finding:** hard-coded default tick size can cause systematic rejects.

**Current code:** ✅ Fixed  
- Tick size starts as `0.0` and quoting is gated until both tokens have known tick size.
- Added public REST “seed tick size” on market discovery.

Files:
- `src/state/book.rs` (tick_size default `0.0`)
- `src/strategy/quote_engine.rs` (gating)
- `src/clients/clob_public.rs` + `src/market_discovery.rs` (seeding)

---

## P1 / Safety + performance

### 6) FeeRateBps mutation safety (concurrency)
**Previous finding:** mutating shared SDK state per order is dangerous under concurrency.

**Current code:** ✅ Fixed  
- `ClobRestClient` wraps signing in `sign_mutex` so feeRateBps + build/sign are serialized.

File:
- `src/clients/clob_rest.rs`

---

### 7) Raw WS logging redaction
**Previous finding:** logging raw frames can leak sensitive data (owner, signature, etc.) and create disk blowups.

**Current code:** ✅ Fixed  
- Raw frame logging now redacts known sensitive keys and truncates payload.

Files:
- `src/clients/clob_ws_user.rs`
- `src/clients/rtds_ws.rs`

---

### 8) Chainlink-vs-Binance divergence shouldn’t halt quoting by default
**Previous finding:** using Chainlink as a “sanity check” can halt quoting during volatility because Chainlink is slower.

**Current code:** ✅ Fixed (good change)  
- Chainlink is the primary; Binance is only fast-move regime input.
- Disagreement is informational; the bot does not automatically halt on disagreement.

File:
- `src/alpha/toxicity.rs`

---

### 9) Merge loop (capital velocity)
**Previous finding:** missing merging means capital locked until expiry.

**Current code:** ✅ Fixed  
- `InventoryEngine` computes full sets and enqueues merge actions.
- Onchain worker executes merge and emits events to reduce inventory.

Files:
- `src/inventory/engine.rs`
- `src/inventory/onchain.rs`
- `src/state/state_manager.rs` (applies merge events)

---

### 10) Orders scoring integration
**Previous finding:** `/orders-scoring` integration missing; can’t optimize rewards.

**Current code:** ✅ Implemented (minimal, safe)  
- `RewardEngine` checks scoring on level-0 orders and nudges size/price.

Files:
- `src/strategy/reward_engine.rs`
- `src/clients/clob_rest.rs::are_orders_scoring`

---

### 11) Log rotation & disk budget
**Previous finding:** 10k buffer + frequent flush could still OOM/disk-fill under sustained WS throughput.

**Current code:** ✅ Fixed / improved  
- Event logger supports rotation and pruning to max_total_bytes.

Files:
- `src/persistence/event_log.rs`
- `src/persistence/runtime.rs`

---

## Still “watch list”

### 12) Expected post-only rejects treated as system failures
**Status:** ⚠️ Partially fixed (still risky)  
- You still apply “failure backoff” when any post rejects, even if the reject is the common post-only race (“would match”).
- Consider classifying rejects by error message and not backoff on expected rejects.

Files:
- `src/clients/clob_rest.rs` (rejection parsing)
- `src/execution/order_manager.rs` (backoff policy)

Patch: `CODE_REVIEW_V2_02_CRITICAL_FIXES.md`

---

### 13) Relayer/Proxy mode for merge/redeem
**Status:** ❌ Not implemented  
- Config explicitly errors if `merge_wallet_mode = relayer`.

Files:
- `src/config.rs`
- `src/inventory/onchain.rs` (EOA only)

Plan included in `CODE_REVIEW_V2_02_CRITICAL_FIXES.md` / `CODE_REVIEW_V2_04_WS_STATE_MANAGEMENT.md` (with minimal trait sketches).
