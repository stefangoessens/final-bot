# TODO

Task status:
- `[ ]` = open
- `[x]` = done

## Wave 0 (Scaffold)
- [x] T0 Repo scaffold + CI + lint

## Wave 1 (Data + State)
- [x] T1 Config system
- [x] T2 State models + StateManager actor
- [x] T3 Gamma client + MarketDiscoveryLoop

## Wave 2 (Feeds)
- [x] T4 CLOB Market WS client (top-of-book + tick size)
- [x] T5 CLOB User WS client (orders + trades)
- [x] T6 RTDS WS client (BTC primary + sanity)

## Wave 3 (Alpha + Strategy)
- [x] T7 AlphaEngine: volatility + drift + probability + toxicity + target_total
- [x] T8 Fee + rebate math module
- [x] T9 QuoteEngine (multi-level ladder + caps + churn rules)
- [x] T10 Inventory risk & completion logic
- [x] T11 StrategyEngine orchestration

## Wave 4 (Execution + Rewards)
- [x] T12 CLOB REST client + completion (FOK/FAK) + heartbeats + feeRateBps caching
- [x] T13 OrderManager (wire REST + state sync)
- [x] T14 Liquidity rewards scoring integration

## Wave 5 (Resolution + Ops + Replay)
- [x] T15 InventoryEngine (Merge + Redeem)
- [x] T16 Observability: metrics + health + structured logging
- [x] T17 Event logging + replay + stress harness

## Wave 6 (Post-review fixes)
- [x] P1 Completion signing safety (feeRateBps + cap rounding + concurrency)
- [x] P2 Startup/shutdown safety (cancel-on-boot + heartbeat graceful shutdown)
- [x] P3 Raw WS logging redaction (no secrets/PII)
- [x] P4 User WS order parsing robustness (partial updates)
- [x] P5 Merge/redeem correctness (e6 units + readiness gates + state sync)
- [x] P6 Size-aware taker EV math (fee depends on Q)

## Wave 7 (Post-review fixes v2)
- [x] W7.1 RTDS Chainlink subscribe payload + watchdog
- [x] W7.2 QuoteTick backpressure removal (StateManager + fanout non-blocking)
- [x] W7.3 Batch post partial-success safety (REST + OrderManager)
- [x] W7.4 Cancels bypass REST post-backoff + execution-layer cutoff suppresses posts
- [x] W7.5 Quoting gates: user WS health + market tradability; restricted => halt
- [x] W7.6 Heartbeats mandatory validation (non-dry-run)
- [x] W7.7 Market discovery grace tracking for previous market + deduped emits
- [x] W7.8 Taker completion wired end-to-end (FOK/FAK + explicit cap + tick-round p_max)
- [x] W7.9 Startup cancel-all fail-closed
- [x] W7.10 Deterministic execution race tests (OrderManager invariants)
- [x] W7.11 Maker-only compliance integration test
- [x] W7.12 Restart reconciliation (seed open orders + inventory from truth)
- [x] W7.13 Market WS per-token staleness gating + config knob
- [x] W7.14 Tick-size readiness gating (do not quote until known)
- [x] W7.15 Liquidity rewards scoring integration end-to-end
- [x] W7.16 Event-log disk budget + rotation + sampling policy
- [ ] W7.17 Replace synthetic reject fixture with captured live user-WS reject frame
- [x] W7.18 Data API mergeable/redeemable readiness integration (remove stubs)

## Wave 8 (Stability hardening)
- [x] W8.1 User WS order-update backpressure (non-blocking forward)
- [x] W8.2 Fee-rate refresh semantics + remove sign_mutex
- [x] W8.3 OrderManager unknown order-update handling (token-aware + rate-limited)

## Wave 9 (Review-driven hardening)
- [x] W9.1 Completion order construction audit (shares semantics + strict cap)
- [x] W9.2 RTDS WS keepalive ping (text PING/PONG)
- [x] W9.3 Execution backoff: ignore expected post-only rejects
- [x] W9.4 Inventory desync watchdog (Data API compare -> cancel+shutdown)
- [x] W9.5 Config validation: reject unimplemented key sources/modes
- [x] W9.6 Cancel terminal failures remove from cache (avoid infinite cancel loop)

## Wave 10 (V2 review backlog)
- [x] W10.1 WS raw-log sampling before redaction (reduce CPU)
- [x] W10.2 Event log: avoid per-event JSON string allocs
- [ ] W10.3 Alpha knob: optional blend with market-implied q
- [x] W10.4 Execution: enforce max_orders_per_min (post limiter)
- [x] W10.5 Completion: pre-completion cancel sweep (REST active-orders cancel)
