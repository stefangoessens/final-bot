# CODE REVIEW (V2) — Execution & Order Lifecycle

This file reviews the execution path end-to-end (strategy → diff → post/cancel → reconciliation) and calls out remaining risks.

---

## What’s strong in the current implementation

### 1) Partial batch safety is fixed
- `ClobRestClient::post_orders_result()` returns both accepted and rejected.
- `OrderManager` uses `post_orders_partial()` and records accepted order IDs even when some rejects occur.

This materially reduces desync risk.

### 2) Signing is serialized
`ClobRestClient` uses `sign_mutex` to guard:
- per-order `feeRateBps` mutation
- build/sign sequence

This is correct given the SDK’s internal mutable state patterns.

### 3) Restart safety is sane by default
- Default `startup_cancel_all = true`
- Optional seeding mode exists (`reconciliation::seed_from_rest()`)

This matches how you prevent “phantom live orders” after restarts.

---

## Remaining execution risks & concrete patches

### P1-1 — `max_orders_per_min` exists in config but is not enforced
`TradingConfig` defines `max_orders_per_min`, but I couldn’t find usage anywhere in `src/`.

**Why it matters:**  
When markets are busy, you can hit Polymarket rate limits (or self-inflict latency) by repeatedly cancel/replacing.

#### Minimal patch: enforce a token-bucket limiter in `OrderManager`
File: `src/execution/order_manager.rs`

Add a limiter:

```rust
use tokio::time::Instant;

#[derive(Debug, Clone)]
struct OrdersPerMinLimiter {
    limit: u32,
    window_start: Instant,
    used: u32,
}

impl OrdersPerMinLimiter {
    fn new(limit: u32) -> Self {
        Self { limit, window_start: Instant::now(), used: 0 }
    }

    fn try_acquire(&mut self, n: u32) -> bool {
        let now = Instant::now();
        if now.duration_since(self.window_start).as_secs() >= 60 {
            self.window_start = now;
            self.used = 0;
        }
        if self.used.saturating_add(n) > self.limit {
            return false;
        }
        self.used += n;
        true
    }
}
```

Store it in `OrderManager`:

```rust
pub struct OrderManager<R: OrderRestClient> {
    // ...
    post_limiter: OrdersPerMinLimiter,
}
```

Initialize with config (when constructing `OrderManager`):

```rust
post_limiter: OrdersPerMinLimiter::new(cfg.trading.max_orders_per_min),
```

And enforce it right before posting:

```rust
let post_count = plan.to_post.len() as u32;

if post_count > 0 && !self.post_limiter.try_acquire(post_count) {
    warn!(
        post_count,
        limit = self.post_limiter.limit,
        "post throttled by max_orders_per_min; skipping this post batch"
    );
    // still allow cancels in this tick; just skip posting
    return Ok(());
}
```

This patch is intentionally minimal; you can later upgrade it to:
- separate “cancel budget” vs “post budget”
- per-token budgets
- adaptive throttling based on 429 responses

---

### P1-2 — “Expected” post-only rejects shouldn’t trip backoff
This is a critical microstructure nuance: even with correct price selection, a fast moving ask can race your post-only bid, causing “would match” rejects.

**Concrete patch is already provided** in `CODE_REVIEW_V2_02_CRITICAL_FIXES.md` (P0-3).

---

### P1-3 — Completion should not assume local cache is perfectly synced
You currently rely on:
- cutoff cancels at T-60s
- then completion window inside T-30s

That’s usually fine, but if cancel requests fail or you have “unknown orders,” completion can still happen while stale maker orders exist.

#### Minimal patch (safe, low frequency):
Before sending completion, do a REST cancel sweep for the market
- fetch active orders for the condition_id
- cancel them
- then place completion order

Code sketch: add to `CompletionExecutor::run()`:

```rust
// pseudo: requires CompletionRequest to include condition_id
let active = rest.get_active_orders_for_market(&req.condition_id).await?;
let ids: Vec<OrderId> = active.iter().map(|o| o.order_id.clone()).collect();
if !ids.is_empty() {
    let res = rest.cancel_orders(&ids).await?;
    if !res.failed.is_empty() {
        warn!(failed = res.failed.len(), "cancel sweep had failures before completion");
    }
}
```

If you choose to implement this, please do it **after** fixing the completion amount semantics (P0-1).

---

## Notes on current diff behavior
Your diff logic:
- cancels mismatched orders
- posts new orders for missing keys
- updates only when (a) price moved by ≥ tick or (b) size changed by threshold

This is a sensible baseline. The 2 biggest “next step” upgrades would be:
1) queue-position aware repricing (hard without exchange metadata)
2) partial cancel/replace (resize) rather than always cancel+new

I’m not proposing those yet because they require deeper behavior assumptions; your current approach is acceptable for the market scope.

