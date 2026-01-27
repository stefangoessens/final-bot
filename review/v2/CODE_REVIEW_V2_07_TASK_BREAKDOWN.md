# CODE REVIEW (V2) — Task Breakdown (actionable, parallelizable)

This is a pragmatic implementation plan with small, well-scoped tasks you can hand to coding agents.

---

## P0 (must fix before production)

### Task P0-A — Fix completion BUY amount semantics (USDC, not shares)
**Owner:** Execution engineer  
**Files:**
- `src/strategy/risk.rs` (extend `TakerAction`)
- `src/strategy/engine.rs` (thread new field)
- `src/execution/completion.rs` (extend request)
- `src/clients/clob_rest.rs` (use `Amount::usdc`)

**Acceptance criteria:**
- BUY completion orders are built with `.amount(Amount::usdc(...))`
- Still uses `.price(p_max)` and `FOK/FAK`

**Test (unit):** add a test for the `max_spend_usdc` computation in `risk.rs`:

```rust
#[test]
fn completion_max_spend_targets_best_ask() {
    let shares = 10.0;
    let best_ask = 0.48;
    let p_max = 0.50;

    let ref_px = best_ask.min(p_max);
    let max_spend = shares * ref_px;

    assert!((max_spend - 4.8).abs() < 1e-9);
}
```

(Then add a small integration test in your CI environment that places a tiny BUY market order in dry-run/sandbox if available.)

---

### Task P0-B — Add RTDS keepalive `PING`
**Owner:** WS engineer  
**File:** `src/clients/rtds_ws.rs`  
**Patch:** see `CODE_REVIEW_V2_02_CRITICAL_FIXES.md` (P0-2)

**Test (unit-ish):** validate your handler recognizes `"PONG"`:

```rust
#[test]
fn rtds_pong_is_recognized() {
    let t = "PONG".to_string();
    assert_eq!(t.trim(), "PONG");
}
```

(Real validation is runtime: ensure RTDS doesn’t disconnect over hours.)

---

### Task P0-C — Do not backoff on expected post-only rejects
**Owner:** Execution engineer  
**Files:**
- `src/clients/clob_rest.rs` (annotate rejects with `expected`)
- `src/execution/order_manager.rs` (use unexpected-only failures)

**Test:** `is_expected_reject` patterns

```rust
#[test]
fn expected_reject_detection() {
    assert!(is_expected_reject("Post only order would match"));
    assert!(is_expected_reject("POST ONLY order would cross"));
    assert!(!is_expected_reject("insufficient balance"));
}
```

---

### Task P0-D — Cancel all orders on graceful shutdown
**Owner:** Platform engineer  
**File:** `src/main.rs`  
**Patch:** see `CODE_REVIEW_V2_02_CRITICAL_FIXES.md` (P0-4)

---

## P1 (strongly recommended)

### Task P1-A — Enforce `max_orders_per_min`
**Owner:** Execution engineer  
**File:** `src/execution/order_manager.rs`  
**Patch:** see `CODE_REVIEW_V2_03_EXECUTION_ORDER_LIFECYCLE.md` (P1-1)

**Test:** limiter window reset behavior

```rust
#[test]
fn limiter_blocks_when_over_limit() {
    let mut lim = OrdersPerMinLimiter::new(3);
    assert!(lim.try_acquire(2));
    assert!(!lim.try_acquire(2)); // would exceed 3
}
```

---

### Task P1-B — Add completion “cancel sweep” safety (optional)
**Owner:** Execution engineer  
**File:** `src/execution/completion.rs`  
**Goal:** before placing completion order, cancel all active orders for that market via REST.

(Implementation sketch in `CODE_REVIEW_V2_03_EXECUTION_ORDER_LIFECYCLE.md`.)

---

### Task P1-C — Inventory reconciliation circuit breaker
**Owner:** State engineer  
**Goal:** if Data API positions differ from local inventory beyond threshold, halt + cancel.

This needs a small design choice (StateManager vs reconciliation module). Once chosen, it’s ~200 lines.

---

## P2 (nice-to-have)

### Task P2-A — Sample raw WS frame logs before redaction
**Owner:** WS engineer  
**Files:**
- `src/clients/clob_ws_user.rs`
- `src/clients/clob_ws_market.rs`
- `src/clients/rtds_ws.rs`

Patch in `CODE_REVIEW_V2_05_PERFORMANCE_LATENCY.md` (P2-1).

---

### Task P2-B — Write JSON logs directly to file (avoid alloc)
**Owner:** Platform engineer  
**File:** `src/persistence/event_log.rs`  
Patch in `CODE_REVIEW_V2_05_PERFORMANCE_LATENCY.md` (P2-2).

---

## Suggested execution order (fastest path to production)
1) P0-A (completion semantics)
2) P0-B (RTDS ping)
3) P0-C (expected rejects)
4) P0-D (shutdown cancels)
5) P1-A (order rate limiter)
6) Paper trading + metrics review
7) P1-C (reconciliation breaker)
8) Performance P2 items

