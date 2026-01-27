# Latency & Performance — Hot Paths, Async Misuse, Churn

**Severity: HIGH**

This section focuses on sub-second reaction time, avoidance of async stalls, and minimizing needless allocations and I/O in hot paths.

## CRITICAL / HIGH issues

### 1) StateManager can block on quote publishing (system-wide backpressure)

- Evidence: `src/state/state_manager.rs` awaits `tx_quote.send(...).await` inside the event loop.
- Why it matters:
  - This blocks processing of **market WS**, **RTDS**, and **user WS** events during downstream congestion.
  - It creates a “priority inversion” where non-critical consumers can starve critical state updates.
- Action:
  - Use a coalescing mechanism (`watch`, or `try_send` + drop) for quote ticks.
  - Consider emitting only a minimal snapshot required for quoting rather than cloning full `MarketState`.

### 2) Excessive cloning and per-tick logging

- Evidence:
  - `QuoteTick` contains a full `MarketState` clone (`src/state/state_manager.rs`).
  - Strategy logs at `info` every tick (`src/strategy/engine.rs::log_desired_summary`).
  - OrderManager logs each planned cancel/post at `info` (`src/execution/order_manager.rs::log_plan`).
- Impact:
  - Cloning `HashMap` + `String` payloads per tick increases CPU and allocator pressure.
  - High-volume logging can dominate runtime, especially under `info` level in production.
- Action:
  - Reduce snapshot size (e.g., `QuoteSnapshot` struct with only required fields).
  - Move tick-level logs to `debug` or add sampling (e.g., log every N ticks or on state change).
  - Keep event-log capture but avoid duplicating the same information via tracing at `info`.

### 3) Blocking file I/O inside tokio task (event logger)

- Evidence: `src/persistence/event_log.rs` flush writes use `std::fs` + `BufWriter` in an async task.
- Impact:
  - On a busy runtime, blocking file writes can steal executor threads and inject latency.
- Action:
  - Use `tokio::task::spawn_blocking` for flush/write work, or a dedicated std thread for log writes.
  - Consider batching larger and reducing flush frequency if you can tolerate more buffered loss.

### 4) Raw WS frame logging is likely too expensive to run at full rate

- Evidence: market WS, user WS, and RTDS WS all log raw frames with JSON redaction/truncation (`src/clients/clob_ws_market.rs`, `src/clients/clob_ws_user.rs`, `src/clients/rtds_ws.rs`).
- Why it matters:
  - Even if logging is non-blocking at the channel boundary, **parsing and redacting JSON per message** can become the hot path at high message rates.
  - This directly competes with timely quote updates.
- Action:
  - Make raw frame logging configurable per feed (market/RTDS/user) and default it off for market+RTDS in production unless actively debugging.
  - If you must keep it for replay, log a structured “canonical event” (already done) and sample raw frames (e.g., 1/N or burst sampling).

## MEDIUM issues / tuning opportunities

1) **MarketDiscovery polls Gamma every second, always emits MarketDiscovered**
   - Evidence: `src/market_discovery.rs` fetches current+next each tick and sends `AppEvent::MarketDiscovered` regardless of change.
   - Impact: unnecessary HTTP + JSON decode; risk of hitting Gamma rate limits; adds noise to StateManager.
   - Action: cache last market identities and only emit when fields change, or only poll aggressively around boundaries.

2) **Intervals without MissedTickBehavior::Skip**
   - Evidence: `MarketDiscoveryLoop` and main timer use default interval behavior.
   - Impact: when stalled, ticks may “catch up” and cause bursts (HTTP spam, exec spam).
   - Action: set `MissedTickBehavior::Skip` for non-critical periodic loops.

3) **Dependency bloat / duplicate HTTP stacks**
   - Evidence: `Cargo.toml` includes `hyper = 0.14` (health/metrics) and `reqwest = 0.12` (hyper 1.x).
   - Impact: larger binary, longer compile time, potentially more TLS stack duplication.
   - Action: consolidate to a single hyper version or use a minimal HTTP server compatible with your reqwest stack.

## Suggested latency instrumentation (actionable)

1) **Measure WS message lag**
   - You already have `ws_message_lag_ms` histogram; wire it:
     - lag = `now_ms - message_timestamp_ms` for market WS and RTDS.
   - This directly validates whether your gating thresholds are realistic.

2) **Measure strategy+execution turnaround**
   - Track per tick:
     - time to build desired,
     - diff computation time,
     - REST submit latency (you already have `order_submit_latency_ms` histogram but it’s not used).
3) **Measure signing+submit critical path**
   - If signing is mutex-protected (it is), track:
     - time spent waiting on `sign_mutex`,
     - time spent in `build+sign`,
     - end-to-end submit RTT.
   - This tells you whether correctness guards are a meaningful latency bottleneck.
