# CODE REVIEW (V2) — P0 / Critical Fixes (with concrete Rust patches)

This file focuses on **issues that can directly cause losses or downtime** and provides concrete code changes.

---

## P0-1 — FIX: Completion BUY “amount” must be USDC, not shares (market orders)

### Why this is critical
Your completion path builds a **BUY market order** like this:

- `src/clients/clob_rest.rs::build_completion_order`  
  - `market_order(token_id, Side::Buy)`  
  - `.amount(Amount::shares(shares))`  ✅ *compile-time ok*  
  - **runtime semantics:** likely wrong

Polymarket docs state for market orders:
- **BUY**: “amount” is **dollars/USDC**
- **SELL**: “amount” is **shares**

References:
- `https://docs.polymarket.com/developers/clob/using-the-clob-client#methods-l1` (see `createMarketOrder` amount comment)
- `https://docs.polymarket.com/developers/clob/orders/create-order-batch` (FOK/FAK market order description)
- The official Rust client examples use `Amount::usdc(...)` for BUY market orders.

### Current code (problem)
In `src/clients/clob_rest.rs`:

```rust
let order = client
    .market_order(token_id, Side::Buy)
    .order_type(map_completion_order_type(order_type))
    .price(price)
    .amount(Amount::shares(shares)) // ❌ BUY market order amount should be USDC
    .build()
    .await?;
```

### Patch plan (minimal, keeps FOK/FAK + explicit cap)
1) Change the completion request to carry a **max spend in USDC**.
2) Build the market order with `.amount(Amount::usdc(...))`.

This preserves your non-negotiables:
- FOK/FAK only
- explicit worst-price cap `p_max`
- no unbounded market orders

### Step A — Extend `TakerAction` to include `max_spend_usdc`
File: `src/strategy/risk.rs`

```rust
#[derive(Debug, Clone)]
pub struct TakerAction {
    pub token_id: String,
    pub p_max: f64,
    pub shares: f64,          // “shares we want to buy” (still useful for metrics)
    pub max_spend_usdc: f64,  // ✅ NEW: what we actually send to createMarketOrder
    pub order_type: CompletionOrderType,
}
```

Inside `should_taker_complete`, compute `max_spend_usdc` from the best ask (to avoid overshooting shares too much):

```rust
let ref_px = best_ask.min(p_max);
let max_spend_usdc = (unpaired * ref_px).max(0.01); // enforce >= 1 cent

return Some(TakerAction {
    token_id: missing_token.to_string(),
    p_max,
    shares: unpaired,
    max_spend_usdc,
    order_type: CompletionOrderType::FAK,
});
```

**Why best_ask instead of p_max?**  
If you compute `max_spend_usdc = shares * p_max`, you can **overshoot shares** when execution price < p_max. Using best_ask targets “roughly shares” and accepts partial fill if price runs away (still reduces risk).

> Uncertainty: docs don’t clearly state whether the “amount” includes fees or fee is charged on top. You should verify with a tiny live test order and capture the resulting fill + balance deltas.

### Step B — Thread `max_spend_usdc` through completion request
File: `src/execution/completion.rs`

```rust
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub slug: String,
    pub token_id: String,
    pub shares: f64,
    pub p_max: f64,
    pub max_spend_usdc: f64,   // ✅ NEW
    pub order_type: CompletionOrderType,
}
```

Update request construction in `src/strategy/engine.rs` where you currently do:

```rust
let req = CompletionRequest {
    slug: state.identity.slug.clone(),
    token_id: action.token_id.clone(),
    shares: action.shares,
    p_max: action.p_max,
    order_type: action.order_type,
};
```

to:

```rust
let req = CompletionRequest {
    slug: state.identity.slug.clone(),
    token_id: action.token_id.clone(),
    shares: action.shares,
    p_max: action.p_max,
    max_spend_usdc: action.max_spend_usdc,
    order_type: action.order_type,
};
```

### Step C — Build BUY market orders with `Amount::usdc(...)`
File: `src/clients/clob_rest.rs`

Change the function signature:

```rust
pub async fn build_completion_order(
    &self,
    token_id: &str,
    max_spend_usdc: f64,   // ✅ replaces shares
    p_max: f64,
    order_type: CompletionOrderType,
    fee_rate_bps: u32,
) -> Result<ClobSignedOrder, BotError> {
    ...
}
```

And the amount field:

```rust
let price = parse_decimal_trunc(p_max, 8);
let usdc = parse_decimal_trunc(max_spend_usdc, 2);

let amount = Amount::usdc(usdc).map_err(|e| {
    BotError::Other(format!("invalid completion usdc amount: {e}"))
})?;

let order = client
    .market_order(token_id, Side::Buy)
    .order_type(map_completion_order_type(order_type))
    .price(price)
    .amount(amount)
    .build()
    .await
    .map_err(BotError::Sdk)?;

let signed = client.sign_order(order, &signer).await.map_err(BotError::Sdk)?;
Ok(signed)
```

Finally update the caller in `CompletionExecutor::run`:

```rust
let signed = rest.build_completion_order(
    &req.token_id,
    req.max_spend_usdc,
    req.p_max,
    req.order_type,
    fee_rate_bps,
).await?;
```

---

## P0-2 — FIX: RTDS WebSocket should send periodic `PING`

### Why it matters
RTDS docs specify the client **should send** `PING` messages periodically (and server responds with `PONG`).  
Reference: `https://docs.polymarket.com/developers/websocket/rtds-overview` (see keepalive section)

Your current `RTDSLoop`:
- responds to WS ping frames
- **does not** send `Message::Text("PING")`

This can cause intermittent RTDS disconnects → stale oracle gating → you stop quoting.

### Patch (minimal)
File: `src/clients/rtds_ws.rs`

Add a ping interval in `read_loop()`:

```rust
use tokio::time::{Duration, MissedTickBehavior};

let mut ping = tokio::time::interval(Duration::from_secs(5));
ping.set_missed_tick_behavior(MissedTickBehavior::Delay);

loop {
    tokio::select! {
        _ = ping.tick() => {
            // RTDS expects text PING/PONG messages
            if let Err(e) = ws.send(Message::Text("PING".to_string())).await {
                return Err(BotError::Ws(format!("RTDS ping send failed: {e}")));
            }
        }

        msg = ws.next() => {
            // existing message handling...
        }

        _ = shutdown.wait() => return Ok(())
    }
}
```

Optional: if you want to confirm health, update a `last_rtds_pong_ms` when you receive `"PONG"` text.

---

## P0-3 — FIX: Don’t backoff on “expected” post-only rejects

### Why it matters
Even with correct logic, a post-only bid can race the book and get rejected as “would match”.  
That should not trigger a “system backoff” that leaves you unquoted.

Right now:
- `ClobRestClient::post_orders_result()` returns `rejected: Vec<PostOrderRejected>`
- `OrderManager` sets `failure_count = outcome.rejected.len()`
- `apply_backoff()` triggers if `failure_count > 0`

### Patch (minimal)
1) Annotate rejects with `expected: bool`.
2) Count only **unexpected** rejects for backoff.

File: `src/clients/clob_rest.rs`

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostOrderRejected {
    pub idx: usize,
    pub error: String,
    pub expected: bool,           // ✅ NEW
    pub order_id: Option<OrderId>,
    pub token_id: Option<String>,
}

fn is_expected_reject(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("post only") && (m.contains("would match") || m.contains("would cross"))
}
```

When building `PostOrderRejected`, set `expected: is_expected_reject(&err_msg)`.

Then in `src/execution/order_manager.rs::apply_post_orders_outcome` compute:

```rust
let unexpected = outcome.rejected.iter().filter(|r| !r.expected).count();
let expected = outcome.rejected.len() - unexpected;

if expected > 0 {
    warn!(expected, "post-only rejects (expected race)");
}
if unexpected > 0 {
    warn!(unexpected, "unexpected order rejects");
}

PostOutcome {
    posted: posted_map,
    rejected: outcome.rejected,
    failure_count: unexpected, // ✅ only unexpected drive backoff
}
```

This keeps your safety backoff for real failures, while staying live through normal microstructure races.

---

## P0-4 — FIX: Graceful shutdown should cancel orders immediately

### Why it matters
You already rely on cancel-on-disconnect heartbeats, but on a planned shutdown (deploys, config changes) you should still **explicitly cancel** to avoid any grace-window exposure.

### Patch
File: `src/main.rs`

Right before returning from `main`, after `shutdown.wait().await`:

```rust
shutdown.wait().await;
tracing::warn!("shutdown requested; canceling all open orders");

if let Some(rest) = maybe_rest.as_ref() {
    if let Err(e) = rest.cancel_all_orders().await {
        tracing::error!(error = %e, "cancel_all_orders failed during shutdown");
    }
}

tracing::info!("shutdown complete");
Ok(())
```

Make sure `maybe_rest` is in scope (or clone the rest client into the shutdown task).

---

## P0-5 — Optional but strongly recommended: Inventory reconciliation “circuit breaker”
Even with the improved restart logic, you still need a **circuit breaker** if inventory desync is detected (REST positions disagree with local state by > X shares).

Minimal implementation sketch:
- every N seconds:
  - fetch positions from Data API for current condition_id
  - compare to `StateManager`’s `inventory`
  - if |diff| > threshold:
    - cancel all orders
    - set `halted=true` and require manual restart

I’m not including the full patch here because it needs a design decision:
- should it live in `StateManager` (cleanest), or in `reconciliation` (simpler)?

If you want, I can provide a full patch once you decide the owner module.

