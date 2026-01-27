# Code Review — Execution & Order Lifecycle

This file audits the “place / cancel / replace / fill / restart” flow.

---

## 1) Order diffing is sane, but watch the backoff triggers

### Where it lives
- `src/execution/order_manager.rs`
  - `diff_orders(...)`
  - `execute_market(...)` (the main per‑tick loop)
  - `execute_cancels(...)`, `execute_posts(...)`

### What it does well
- **Keyed order cache** uses `(token_id, level)` which matches your ladder design.
- Per tick:
  - cancel orders that are no longer desired,
  - post missing orders,
  - reprice/resize if price moved more than N ticks or size drifted more than pct.
- Uses **batching** for cancels/posts (via `max_cancel_batch`, `max_post_batch`).

### Practical risk: “post-only rejects” are treated like errors
Expected rejects (post-only would cross) will happen in fast move. If these increment `failures_count`, you’ll start applying backoff and “self-disable” quoting.

**Actionable change**: classify rejects and only backoff on unexpected ones.

```rust
fn is_expected_reject(err: &str) -> bool {
    let e = err.to_lowercase();
    e.contains("post-only") || e.contains("post only") || e.contains("would cross") || e.contains("cross")
}
```

Integrate this into `apply_post_orders_result(...)` so that `failures_count` ignores those.

---

## 2) Restart safety: good options, but the “seed mapping” has edge cases

### Current implementation
- `main.rs`:
  - `startup_cancel_all` (hard reset) OR
  - `StartupReconciler::run()` (seed open orders + seed inventory) (`src/reconciliation.rs`)

### What’s good
- You **can cancel everything** on startup — safest if you’re okay with a short downtime.
- The reconciler:
  - Fetches open orders and filters for the current condition and BUY side.
  - Groups by token and assigns ladder `level` by sorting prices descending.
  - Cancels sell orders and irrelevant orders (optional).

### Edge case to be aware of
If you change `ladder_levels` or quote spacing, the “seeded” level mapping can be misaligned with your new config. That’s okay because your next diff tick will cancel/replace into the new ladder, but you might:
- temporarily keep too many levels, or
- cancel the wrong “intended level” first.

**If you value absolute determinism:** prefer `startup_cancel_all=true` for production deployments.

---

## 3) User WS order updates and order cache

### Flow
- `src/clients/clob_ws_user.rs` parses order updates into `UserOrderUpdate`.
- `OrderManager::apply_user_order_update` updates order cache by `order_id`.

### Robustness improvements
If `apply_user_order_update` can’t map `order_id` -> `(token_id, level)` it logs and ignores. That’s okay if startup reconciliation is always correct, but there are still scenarios:
- an order was placed but the process crashed before it recorded the mapping,
- old order updates arrive after a reconnect before reconciliation completes.

**Safer behavior**: if you get too many “unknown order_id” events in a short window, trigger:
- `cancel_all_orders()` and shutdown OR
- fetch open orders for condition id and rebuild cache.

Minimal code example:

```rust
// in OrderManager
unknown_order_updates: u32,

fn apply_user_order_update(&mut self, update: UserOrderUpdate) {
    if self.key_for_order_id(&update.order_id).is_none() {
        self.unknown_order_updates += 1;
        if self.unknown_order_updates > 50 {
            self.shutdown.trigger("unknown_order_updates");
        }
        return;
    }
    self.unknown_order_updates = 0;
    // existing update logic...
}
```

---

## 4) Completion executor: fix needed (see critical fixes)

- `execution/completion.rs` calls `ClobRestClient::build_completion_order(...)`.
- That builder currently uses wrong semantics for BUY market orders (see `CODE_REVIEW_01_CRITICAL_FIXES.md`).

Once fixed, the completion flow is OK:
- It stops maker quoting for that tick (`StrategyEngine` sends empty desired).
- It submits a bounded FOK/FAK order with explicit price cap.

---

## 5) Quote cutoff 60s before end: correct and enforced

- `StateManager` computes `cutoff_ts_ms = end_ts_ms - 60_000`.
- `QuoteEngine` returns empty desired orders if `now >= cutoff`.
- `OrderManager` sees empty desired and cancels all live orders.

This matches your non-negotiable.

---

## 6) Concrete production hardening steps

1. **Fix completion order construction** (must).
2. Add “expected reject” classification to reduce unnecessary backoff.
3. Add tick-size gating (`tick_size_unknown` block reason).
4. Add an “unknown order update storm” shutdown guard.
5. Consider a periodic open-orders reconciliation (slow path) every ~60s:
   - fetch active orders for condition id,
   - compare to local cache,
   - if mismatch: cancel + rebuild, or exit.

Example scaffold:

```rust
async fn periodic_open_order_reconcile(rest: &ClobRestClient, condition_id: &str) -> BotResult<()> {
    let open = rest.get_active_orders_for_market(condition_id).await?;
    // compare with self.cache.live ...
    Ok(())
}
```
