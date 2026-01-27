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
