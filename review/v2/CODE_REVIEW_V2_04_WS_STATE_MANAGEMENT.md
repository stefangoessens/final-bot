# CODE REVIEW (V2) — WebSockets & State Management

This file reviews your WS ingestion + state fusion pipeline and highlights remaining risks.

---

## Market WS (`src/clients/clob_ws_market.rs`)

### What’s good
- Reconnect loop with backoff.
- Subscriptions are driven by market discovery; token IDs are maintained in a shared set.
- You now support **tick size change events** and propagate them to state.
- You track staleness and gate quoting until the book is fresh.

### What I’d watch
- You only store **top-of-book** (best bid/ask). That’s OK for this strategy, but it makes:
  - microprice / imbalance alpha harder
  - “quote through depth” impossible
- If Polymarket introduces additional event types, your parser currently ignores unknown events (good) but logs can spike.

No mandatory changes here.

---

## User WS (`src/clients/clob_ws_user.rs`)

### What’s good
- You actively send `PING` every 10 seconds and respond to ping frames.
- Order update parsing is more robust (optional fields; missing price doesn’t crash).
- Raw frame logging is redacted + truncated.

### What I’d watch
- “Unknown order_id” updates still occur in real life (network blips, restart windows).  
  Your default `startup_cancel_all=true` makes this less dangerous, but it’s still worth monitoring.

---

## RTDS WS (`src/clients/rtds_ws.rs`)

### Current gap
- You **do not send** periodic text `PING` messages.
- RTDS docs explicitly recommend sending `PING` every ~5 seconds.

✅ A concrete patch is provided in `CODE_REVIEW_V2_02_CRITICAL_FIXES.md` (P0-2).

### Optional improvement: track PONG to drive health
If you want RTDS health to reflect actual keepalive round-trips, add:

```rust
// inside message handler
if let Message::Text(t) = msg {
    if t.trim() == "PONG" {
        health.set_rtds_ok();
        continue;
    }
}
```

This avoids false “healthy” states where the socket is open but the feed is stalled.

---

## StateManager gating (`src/state/state_manager.rs`)

### What’s good
- Clear staleness model:
  - requires market ws freshness
  - requires user ws heartbeat (in non-dry-run)
  - requires Chainlink freshness
- Cutoff logic is explicit: `cutoff_ts_ms = end - 60s`.
- Exports a single `MarketState` tick that downstream engines use.

### Remaining risk: no global “circuit breaker” for inventory desync
If:
- onchain merge/redeem succeeds but an event is dropped, or
- user ws fill stream lags for an extended period

…the bot can end up quoting with a false inventory picture.

I recommend a small reconciliation circuit breaker (see P0-5 in `CODE_REVIEW_V2_02_CRITICAL_FIXES.md`).

---

## Summary
Your WS + state design is now structurally solid. The only WS-level P0 gap I see is RTDS keepalive.

