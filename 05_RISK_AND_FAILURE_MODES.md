# Risk & Failure Modes — Kill Switches, Staleness, Rollover

**Severity: CRITICAL**

This section evaluates whether the bot fails safe under common adverse conditions: WS stalls, partial REST failures, market close/rollover, and compliance restrictions.

## Principle: make safety “execution-layer hard”

Anything that must never happen (e.g., placing orders after cutoff, quoting while blind to fills) must be enforced where orders are actually placed/canceled — not only in the quote engine.

## CRITICAL failure modes

### 1) User WS disconnect does not halt quoting (spec violation)

- Evidence:
  - `FULL_SPEC.md` explicitly requires: “If user WS disconnects: pause quoting…”
  - Current quoting gate (`MarketState.quoting_enabled`) ignores user WS health (`src/state/state_manager.rs`).
  - HealthState tracks user WS staleness (`src/ops/health.rs`) but that’s only surfaced via `/healthz`.
- Impact:
  - Bot can keep quoting while blind to fills and order cancellations → inventory drift → catastrophic exposure.
- Action:
  - Make user WS health a hard prerequisite for quoting when not dry-run.
  - On user WS stale: cancel orders + pause quoting (or halt).

### 2) REST backoff can prevent cancellations

- Evidence: `src/execution/order_manager.rs` skips all execution during backoff.
- Impact: in “must cancel” events (cutoff, stale oracle, shutdown), orders can remain open.
- Action: cancels must bypass backoff; optionally escalate to `cancel_all_orders` on repeated failures.

### 3) Ended markets can be dropped before redeem/cleanup completes

- Evidence:
  - `MarketDiscoveryLoop` tracks only current+next and emits `SetTrackedMarkets`, causing StateManager to `retain()` only those slugs.
  - `InventoryEngine` redemption relies on `MarketIdentity.closed`, but that flag stops updating when a market is no longer polled.
- Impact:
  - Positions can remain stuck (no redeem), inventory state can become permanently inconsistent.
- Action:
  - Introduce “post-close tracking” for ended markets until redemption succeeds.
  - Base redemption scheduling primarily on `interval_end_ts` + onchain readiness, not Gamma `closed` alone.

### 4) Missing compliance/geoblock kill switch

- Evidence:
  - Spec requires immediate cancel+halt on restriction detection.
  - There is no handling of 403/451/“restricted” API responses in Gamma/REST paths, and `restricted` flag from Gamma is not used to halt.
- Impact: compliance risk + potential account restriction escalation.
- Action:
  - Treat `MarketIdentity.restricted == true` as an immediate HALT + cancel-all.
  - Detect HTTP status codes / known error bodies indicating restriction and halt.

## HIGH risk gaps

1) **No inventory cap enforcement as a hard stop**
   - Spec mentions `max_unpaired_shares_global` as a kill trigger; code only uses it as a soft limit in skew logic.
   - Action: implement hard halt when global skew exceeds cap.

2) **Staleness gating likely miscalibrated**
   - Market WS staleness currently uses `alpha_cfg.rtds_stale_ms` (default 750ms) rather than `market_ws_stale_ms`.
   - Action: add explicit `market_ws_stale_ms` and validate against observed WS cadence.

3) **Proxy/relayer merge mode is unimplemented**
   - Evidence: `src/inventory/onchain.rs` logs that `RELAYER` is not implemented and disables merges/redeems.
   - Impact: capital velocity mechanism may silently not run in the intended deployment mode.
   - Action: either implement relayer mode or make config validation fail hard when set.

## Suggested failure-injection tests (actionable)

1) **WS stall test**
   - Drop market WS traffic for > `market_ws_stale_ms`, verify:
     - quoting disabled within threshold,
     - cancels are successfully sent even if REST errors occur.

2) **User WS stall test**
   - Drop user WS traffic, verify:
     - quoting halts,
     - orders are canceled,
     - bot does not resume until user WS healthy.

3) **Partial batch failure test**
   - Force one order in a batch to fail, verify accepted orders are tracked and later canceled/replaced safely.

## Kill-switch matrix (what should latch and what it should do)

Each kill switch should be **latching** (requires explicit reset) unless you intentionally design an auto-resume state machine.

Minimum set to implement/verify:

1) **Approaching expiry** (`now_ms >= cutoff_ts_ms`)
   - Cancel all orders for that market.
   - Block any new placements even if strategy emits quotes.

2) **User WS stale / disconnected**
   - Cancel all orders (or at least block new placements + cancel best-effort).
   - Pause quoting until user WS healthy again (or halt).

3) **Chainlink stale (settlement oracle stale)**
   - Cancel all orders in affected markets.
   - Pause quoting until Chainlink is fresh again.

4) **Market WS stale / desync**
   - Cancel orders on stale token(s) and pause quoting for that market until resynced.

5) **Restricted / compliance**
   - Cancel all and **halt**.

6) **Inventory cap exceeded**
   - Cancel all and halt (capital preservation posture).

7) **Heartbeat supervisor failure**
   - Cancel all and stop placing orders (already implemented, but ensure it can’t be bypassed by other loops).
