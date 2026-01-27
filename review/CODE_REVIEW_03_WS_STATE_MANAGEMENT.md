# Code Review — WebSockets, State Management, and Desync Risks

---

## 1) Market WS (`src/clients/clob_ws_market.rs`)

### What’s good
- Handles:
  - `best_bid_ask` updates (top-of-book),
  - `price_change` updates,
  - `tick_size_change` updates.
- Maintains a `last_best` map to handle events that don’t include full context.
- Sends subscribe/unsubscribe based on `MarketDiscoveryUpdate`.

### Risks / recommendations
1. **Keepalive / ping behavior isn’t documented clearly** in official docs.
   You’re not currently sending text `PING` on the market channel. If the server expects it, you’ll see intermittent disconnects.
2. Add a ping loop (same approach as RTDS/user WS) and handle `PONG`.

Concrete patch pattern: same as in `CODE_REVIEW_01_CRITICAL_FIXES.md` (split read/write and send `"PING"` every N seconds).

---

## 2) User WS (`src/clients/clob_ws_user.rs`)

### What’s good
- Implements a text `PING` keepalive task.
- Parses:
  - order updates into `UserOrderUpdate` (partial fields are optional),
  - trade events into `FillEvent`.

### Edge cases to test
- Ensure you can parse:
  - order updates where `price` or `original_size` are missing (you already use `Option`),
  - trade events where fields are strings vs numbers (you accept both via `de_f64`).

Suggested test additions:
- Add JSON fixtures based on real payloads for:
  - partial update,
  - cancel event,
  - reject event,
  - trade event.

---

## 3) RTDS WS (`src/clients/rtds_ws.rs`)

### What’s good
- Separates “primary” (binance) and “sanity” (chainlink) price updates.
- Feeds into alpha + toxicity gating.

### Must improve
- Add explicit keepalive `PING` (see critical fixes) since RTDS docs recommend it.

---

## 4) StateManager (`src/state/state_manager.rs`)

### What’s good
- Single-threaded event application avoids data races.
- Health gating:
  - user ws stale disables quoting,
  - chainlink stale disables quoting via alpha size_scalar.

### Key recommendation: add tick-size gating
Right now quoting can start before tick size seed arrives. Add a `tick_size_unknown` block reason so you don’t place invalid prices.

Concrete snippet is in `CODE_REVIEW_01_CRITICAL_FIXES.md`.

---

## 5) Inventory desync risk & recommended mitigation

- Inventory is updated from fill events (user WS).
- Startup inventory is seeded from Data API (reconciliation step).

If you miss a fill event without noticing, local inventory becomes wrong. The **safest** mitigation is a “desync watchdog”:
- compare local vs Data API,
- if mismatch beyond threshold: cancel + shutdown (do not overwrite live state).

Concrete code skeleton is in `CODE_REVIEW_01_CRITICAL_FIXES.md`.

---

## 6) Practical “failure mode matrix” (what should happen)

| Failure | Current behavior | Risk | Recommended change |
|---|---|---|---|
| Market WS disconnect | reconnect loop | brief staleness -> quoting may continue | ok; optionally add pings |
| User WS disconnect | staleness disables quoting (block reason) | orders could remain if cancels fail but heartbeats keep alive | after N seconds stale: shutdown to stop heartbeats |
| RTDS disconnect | chainlink stale -> size_scalar=0 -> quoting disabled | downtime until reconnect | add ping + faster reconnect |
| REST post errors | backoff | self‑inflicted downtime if errors are “expected rejects” | classify rejects |
| Onchain RPC down | merge loop errors | slower capital velocity | ok; do not block quoting |

---

## Suggested “user ws stale => fatal” guard (optional but safer)

Instead of just disabling quoting, consider exiting after a grace period:

```rust
// inside StateManager recompute:
if !health.user_ws_fresh && now_ms() - self.health.last_user_ws_ms.load(Ordering::Relaxed) > 15_000 {
    // emit fatal via a channel, or set a shutdown flag
}
```

This ensures you don't run “blind” while orders might still exist.
