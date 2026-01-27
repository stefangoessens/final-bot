# CODE REVIEW (V2) — Performance & Latency

This file highlights hot paths and the most leverageful performance fixes for a 2‑market, sub‑second tick strategy.

---

## Where latency matters most
For this bot, the most important latency loops are:

1) **Market WS updates → StateManager tick**  
   Any lag here makes you quote stale.

2) **StateManager tick → desired orders → OrderManager**  
   If you fall behind, you churn orders and risk rate limiting.

3) **OrderManager post/cancel → user WS updates**  
   Determines how quickly your local view reflects reality.

4) **RTDS update freshness**  
   If Chainlink becomes “stale” due to WS issues, you stop quoting entirely.

---

## Good decisions already present
- Bounded channels + “drop if lagging” behavior prevent unbounded memory growth.
- Event logger does burst draining and uses buffered IO.
- Raw WS frame logging is redacted/truncated (security > perf, but still important).

---

## Performance improvements (with concrete code)

### P2-1 — Avoid JSON parse/redaction on every raw WS message (sample before redact)
Right now, `log_raw_frame()` parses JSON and redacts fields even if downstream sampling will drop it.

This can become expensive if WS rate spikes.

#### Patch: add a cheap counter sampler in WS loops
Example: `src/clients/clob_ws_user.rs`

Add a counter field:

```rust
pub struct UserWsLoop {
    // ...
    raw_log_every_n: u64,
    raw_log_counter: u64,
}
```

Initialize with a config knob (or a constant):

```rust
raw_log_every_n: 100, // log ~1% if 10k msgs
raw_log_counter: 0,
```

Then wrap the call:

```rust
self.raw_log_counter += 1;

if self.raw_log_every_n > 0 && (self.raw_log_counter % self.raw_log_every_n == 0) {
    self.log_raw_frame(&text).await;
}
```

Do the same for:
- `src/clients/clob_ws_market.rs`
- `src/clients/rtds_ws.rs`

This preserves debuggability while making worst-case load predictable.

---

### P2-2 — Avoid allocating a JSON string per event log line
In `src/persistence/event_log.rs`, if you currently do `serde_json::to_string`, you allocate a new `String` every event.

#### Patch: write JSON directly to the writer
Replace:

```rust
let line = serde_json::to_string(&event)?;
writer.write_all(line.as_bytes())?;
writer.write_all(b"\n")?;
```

with:

```rust
serde_json::to_writer(&mut *writer, &event)?;
writer.write_all(b"\n")?;
```

This is a pure win under high throughput.

---

### P2-3 — Tokenize expensive log messages
Several `warn!` / `info!` logs format large strings in hot loops. Prefer structured fields:

```rust
warn!(
    token_id = %token_id,
    best_bid = best_bid,
    best_ask = best_ask,
    "book update"
);
```

This is already the style you use in much of the code; just keep it consistent in the WS loops.

---

## Summary
You’re already in a decent place performance-wise given the small market scope.  
The two patches above are the “cheap wins” that reduce CPU and allocator pressure without changing strategy behavior.

