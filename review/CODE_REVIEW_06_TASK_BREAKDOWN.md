# Code Review — Task Breakdown (parallelizable)

This is an execution plan for a small agent team. Each task includes files + acceptance criteria.

---

## P0 — Blocking correctness fixes

### Task P0.1 — Fix completion order construction
- **Files:** `src/clients/clob_rest.rs`
- **Change:** update `build_completion_order` to build a marketable limit (shares) + FOK/FAK + price cap.
- **Acceptance:** completion orders buy exactly `shares` (or fail/partial based on FOK/FAK) and never exceed `p_max`.
- **Notes:** Use the patch in `CODE_REVIEW_01_CRITICAL_FIXES.md`.

### Task P0.2 — Add RTDS WS keepalive
- **Files:** `src/clients/rtds_ws.rs`
- **Change:** split read/write; send text `PING` periodically; handle text `PONG`.
- **Acceptance:** RTDS stays connected for long sessions; `last_chainlink_ms` stays fresh.

---

## P1 — Safety hardening

### Task P1.1 — Gate quoting on tick size known
- **Files:** `src/state/state_manager.rs`
- **Change:** introduce `tick_size_unknown` block reason when `tick_size <= 0` for either token.
- **Acceptance:** No orders posted before tick sizes seeded; no startup tick-size rejects.

### Task P1.2 — Don’t backoff on expected post-only rejects
- **Files:** `src/execution/order_manager.rs`
- **Change:** parse rejected errors; ignore post-only/would-cross in backoff decision.
- **Acceptance:** volatility periods don’t cause long backoff pauses solely due to post-only rejects.

### Task P1.3 — Add inventory desync watchdog (safe-mode)
- **Files:** new `src/inventory/desync_watchdog.rs`; wire in `src/main.rs`
- **Change:** poll Data API positions; compare to local; if mismatch > threshold, cancel + shutdown.
- **Acceptance:** silent WS fill loss triggers shutdown quickly; normal operation unaffected.

### Task P1.4 — User WS stale escalation
- **Files:** `src/state/state_manager.rs` (or a new watchdog task)
- **Change:** if user ws stale > N seconds, trigger fatal shutdown rather than staying alive.
- **Acceptance:** bot does not “run blind” for extended periods.

---

## P2 — Execution polish

### Task P2.1 — Unknown order update storm handling
- **Files:** `src/execution/order_manager.rs`
- **Change:** if too many unknown order IDs in short period, trigger cancel_all + shutdown or REST reconcile.
- **Acceptance:** bot self-recovers from cache corruption.

### Task P2.2 — Periodic open-orders reconcile (slow path)
- **Files:** `src/execution/order_manager.rs` or new module
- **Change:** every 30–60s, fetch active orders for current conditions and compare to cache.
- **Acceptance:** local/exchange drift is detected.

---

## P3 — Strategy improvements (non-blocking)

### Task P3.1 — Blend model prob with market-implied mid
- **Files:** `src/alpha/mod.rs`
- **Change:** compute `q_up_blended` = weighted average of GBM model and up-mid.
- **Acceptance:** bot becomes less “model rigid”; evaluate impact via logs.

### Task P3.2 — Volatility-based ladder widening
- **Files:** `src/strategy/quote_engine.rs`
- **Change:** spread/level spacing responds to `alpha.regime` or `sigma`.
- **Acceptance:** adverse selection reduced in fast-move regimes.

### Task P3.3 — Backtest harness (replay event logs)
- **Files:** new `src/backtest/*` (or a separate crate)
- **Change:** replay recorded events into StateManager/Strategy/Execution to measure PnL.
- **Acceptance:** deterministic runs; produces summary metrics.

---

## P4 — Ops / production deployment

### Task P4.1 — Key management validation
- **Files:** `src/config.rs`, `src/main.rs`
- **Change:** either implement SecretsManager/KMS loading or reject unsupported modes in validation.
- **Acceptance:** config cannot claim a mode that isn’t implemented.

### Task P4.2 — Logging rotation / disk budget test
- **Files:** `src/persistence/*`
- **Change:** add integration test or runtime check that rotation and max bytes work.
- **Acceptance:** disk usage bounded under heavy WS throughput.

---

## Suggested agent parallelization

- **Agent A:** P0.1 + P2.1
- **Agent B:** P0.2 + P1.1
- **Agent C:** P1.2 + P1.4
- **Agent D:** P1.3 (watchdog) + P4.2
- **Agent E:** P3.* (strategy/backtest, non-blocking)
