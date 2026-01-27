# TASK_BREAKDOWN.md
# Polymarket BTC 15m Market-Maker Bot (Rust) - PARALLEL TASK BREAKDOWN v1.1 (Alpha-first)

Last updated: 2026-01-26

Rules:
- Each task owns specific files/modules to avoid conflicts.
- Tasks communicate via explicit interfaces (types, traits, channels).
- "Done" means tests + docs + integration points.

------------------------------------------------------------
Wave 0 (Scaffold) - must land first
------------------------------------------------------------

T0) Repo scaffold + CI + lint
Owner files:
- Cargo.toml, src/main.rs, src/error.rs
- ops/logging.rs
- .github/workflows/*

Goal:
- Create compilable Rust project with tokio, serde, tracing.
- Add formatting/lint (rustfmt, clippy) in CI.

Interfaces:
- main.rs defines module skeleton and starts tokio runtime.
- error.rs defines BotError.

Acceptance:
- `cargo test` passes
- `cargo clippy -- -D warnings` passes
- logs print boot banner

------------------------------------------------------------
Wave 1 (Data + State) - can be parallel after T0
------------------------------------------------------------

T1) Config system
Owner files:
- src/config.rs

Goal:
- Define config structs and load from env + optional config file.
- Provide defaults and validation.

Interfaces:
- pub fn load_config() -> Result<AppConfig, BotError>
- AppConfig includes sections: trading, inventory, alpha, rewards, infra, keys.

Acceptance:
- unit test: missing required keys fails with clear message
- unit test: defaults load

Dependencies:
- T0

---

T2) State models + StateManager actor
Owner files:
- src/state/mod.rs
- src/state/market_state.rs
- src/state/order_state.rs
- src/state/inventory.rs
- src/state/book.rs
- src/state/rtds_price.rs
- src/state/state_manager.rs (if split)

Goal:
- Create canonical in-memory state and an event-application API.
- Define event enums for feed updates and order/fill updates.

Interfaces:
- enum AppEvent { MarketDiscovered(...), MarketWsUpdate(...), UserWsUpdate(...), RTDSUpdate(...), TimerTick(...) }
- StateManager: async fn run(rx: Receiver<AppEvent>, tx_quote: Sender<QuoteTick>)

Acceptance:
- unit test: applying fill updates cost basis correctly
- unit test: staleness detection fields updated

Dependencies:
- T0, T1

---

T3) Gamma client + MarketDiscoveryLoop
Owner files:
- src/clients/gamma.rs
- src/time.rs (interval helpers)
- src/market_discovery.rs

Goal:
- Compute slugs for current+next and fetch Gamma metadata.
- Emit MarketDiscovered events to StateManager.

Interfaces:
- struct GammaClient { ... }
- async fn fetch_market_by_slug(slug) -> MarketIdentity
- MarketDiscoveryLoop: async fn run(tx_events)

Acceptance:
- unit test: slug computation for known timestamps
- integration (optional): fetch returns condition_id + token ids

Dependencies:
- T0, T1, T2

------------------------------------------------------------
Wave 2 (Feeds) - can be parallel after Wave 1
------------------------------------------------------------

T4) CLOB Market WS client (top-of-book + tick size)
Owner files:
- src/clients/clob_ws_market.rs

Goal:
- Connect to wss://ws-subscriptions-clob.polymarket.com/ws/market
- Subscribe to token IDs from StateManager commands (or from config initially).
- Parse best_bid_ask and tick_size_change messages and publish MarketWsUpdate events.

Interfaces:
- MarketWsLoop::run(rx_cmd: Receiver<MarketWsCommand>, tx_events: Sender<AppEvent>)
- enum MarketWsCommand { Subscribe(Vec<String>), Unsubscribe(Vec<String>) }

Acceptance:
- reconnect with backoff works
- staleness timestamp updates
- handles message parse errors without crash

Dependencies:
- T0, T2

---

T5) CLOB User WS client (orders + trades)
Owner files:
- src/clients/clob_ws_user.rs

Goal:
- Connect to wss://ws-subscriptions-clob.polymarket.com/ws/user
- Auth subscribe with apiKey/secret/passphrase.
- Parse order + trade messages, publish UserWsUpdate events.

Interfaces:
- UserWsLoop::run(tx_events)
- Must accept optional "markets filter" list

Acceptance:
- reconnect works
- emits Fill events with price/size/order_id/token_id

Dependencies:
- T0, T1, T2

---

T6) RTDS WS client (Binance + Chainlink; oracle health)
Owner files:
- src/clients/rtds_ws.rs

Goal:
- Connect to wss://ws-live-data.polymarket.com
- Subscribe to:
  - crypto_prices update filters btcusdt
  - crypto_prices_chainlink * filters {"symbol":"btc/usd"}
- Publish RTDSUpdate events.

Interfaces:
- RTDSLoop::run(tx_events)
- RTDSUpdate includes: source, price, ts_ms

Acceptance:
- Chainlink and Binance price + staleness tracking available in state
- Chainlink is treated as settlement oracle input (primary for alpha)
- Binance is treated as leading indicator only (fast move detection)
- reconnect works

Dependencies:
- T0, T2

------------------------------------------------------------
Wave 3 (Alpha + Strategy) - can be parallel once state exists
------------------------------------------------------------

T7) AlphaEngine: volatility + drift + probability + toxicity + target_total
Owner files:
- src/alpha/volatility.rs
- src/alpha/probability.rs
- src/alpha/toxicity.rs
- src/alpha/target_total.rs
- src/alpha/mod.rs

Goal:
- Implement deterministic alpha described in FULL_SPEC.
- Compute q_up, caps, regime, dynamic target_total.

Interfaces:
- fn update_alpha(market_state: &mut MarketState, now_ms: i64, alpha_cfg: &AlphaConfig, oracle_cfg: &OracleConfig, trading_cfg: &TradingConfig)

Acceptance:
- unit tests:
  - fee table probability sanity: if St > S0 and tau small -> q_up > 0.5
  - if Chainlink stale -> regime STALE_ORACLE
  - if Binance stale (but Chainlink healthy) -> regime STALE_BINANCE (size reduced, but not halted)
  - oracle disagree is WARNING only (no halt by itself)
  - target_total within bounds

Dependencies:
- T2, T6

---

T8) Fee + rebate math module
Owner files:
- src/strategy/fee.rs

Goal:
- Implement taker fee formula:
  fee = shares * price * 0.25 * (price*(1-price))^2
- Provide helper for fee_per_share and rebate weight.

Interfaces:
- fn taker_fee_usdc(shares: f64, price: f64) -> f64
- fn fee_equivalent(shares: f64, price: f64) -> f64

Acceptance:
- unit tests match maker rebates table points:
  - price=0.5, shares=100 => ~0.78125 fee (rounded behavior documented)
  - symmetric at 0.5 +/- x

Dependencies:
- T0

---

T9) QuoteEngine (multi-level ladder + caps + churn rules)
Owner files:
- src/strategy/quote_engine.rs
- src/strategy/mod.rs

Goal:
- Convert MarketState snapshot into DesiredOrders for Up/Down with ladder.
- Enforce combined cap, tick rounding, post-only, and churn thresholds.

Interfaces:
- fn build_desired_orders(state: &MarketState, cfg: &TradingConfig, now_ms: i64) -> Vec<DesiredOrder>

Acceptance:
- unit tests:
  - combined cap always satisfied at level0
  - prices always align to tick
  - post-only constraint enforced

Dependencies:
- T2, T7

---

T10) Inventory risk & completion logic
Owner files:
- src/strategy/risk.rs

Goal:
- Implement skew ladder and emergency completion policy.
- Uses cost basis + fee module to decide taker completion.

Interfaces:
- fn adjust_for_inventory(desired: &mut Vec<DesiredOrder>, state: &MarketState, cfg: &InventoryConfig, now_ms: i64)
- fn should_taker_complete(...) -> Option<TakerAction>

Acceptance:
- unit tests for:
  - skew_moderate removes excess side level0
  - taker completion only when profit_per_share >= threshold

Dependencies:
- T2, T8, T9

---

T11) StrategyEngine orchestration
Owner files:
- src/strategy/engine.rs

Goal:
- On QuoteTick, run alpha update then quote build then risk adjust.
- Send DesiredOrders to OrderManager.

Interfaces:
- StrategyEngine::run(rx_quote_tick, tx_exec_cmd)

Acceptance:
- integration test: with fake state updates, desired orders emitted

Dependencies:
- T2, T7, T9, T10

------------------------------------------------------------
Wave 4 (Execution + Rewards) - can be parallel after strategy skeleton
------------------------------------------------------------

T12) CLOB REST client + order posting/canceling + completion + feeRateBps caching + heartbeats
Owner files:
- src/clients/clob_rest.rs

Goal:
- Implement REST calls needed:
  - POST /order (and batch)
  - DELETE /order (and batch)
  - GET /fee-rate?token_id=
  - GET /data/orders (active orders)
- Completion orders:
  - Support OrderType.FOK and OrderType.FAK with explicit limit cap P_max
- Heartbeats (mandatory):
  - Enable the official CLOB heartbeat / cancel-on-disconnect mechanism
- Cache fee_rate_bps per token with TTL.

Interfaces:
- async fn get_fee_rate_bps(token_id) -> u64
- async fn post_orders(batch) -> Result<Vec<OrderId>, BotError>
- async fn cancel_orders(order_ids) -> Result<CancelResult, BotError>

Acceptance:
- unit test: caching returns same value within ttl
- integration (optional): GET fee-rate works

Dependencies:
- T0, T1

---

T13) OrderManager (diff + batch + debounce)
Owner files:
- src/execution/order_manager.rs
- src/execution/mod.rs
- src/execution/batch.rs

Goal:
- Diff DesiredOrders vs live orders, apply churn rules, execute cancel/post via CLOB REST client.
- Emit events back to StateManager.

Interfaces:
- OrderManager::run(rx_exec_cmd, tx_events)
- ExecCommand includes market_slug + desired orders list

Acceptance:
- unit tests for diff logic
- handles partial fills and avoids duplicate orders

Dependencies:
- T2, T11, T12

---

T14) Liquidity rewards scoring integration
Owner files:
- src/strategy/reward_engine.rs

Goal:
- Implement /orders-scoring checks for open orders and adjust quoting if enabled.

Interfaces:
- RewardEngine::run(periodic tick, query open orders, call scoring endpoint, emit hints/events)

Acceptance:
- if scoring required and not scoring, engine suggests changes (size/price) within constraints

Dependencies:
- T12, T2

------------------------------------------------------------
Wave 5 (Resolution + Ops + Replay)
------------------------------------------------------------

T15) InventoryEngine (Merge + Redeem)
Owner files:
- src/inventory/mod.rs
- src/inventory/engine.rs
- src/resolution/mod.rs
- src/resolution/redeemer.rs

Goal:
- Implement capital-velocity merge loop + post-resolution redeem.
Why:
- MergePositions burns full sets for collateral anytime after condition prepared.

Scope:
- Track full_sets = min(pos_up, pos_down)
- Merge when >= merge_min_sets with batching/rate limits
- Wallet-mode-aware gas policy (Relayer vs EOA)

Interfaces:
- InventoryEngine::tick(now_ms, market_states_snapshot) -> Vec<InventoryAction>
- InventoryAction:
  - Merge { condition_id, qty_sets }
  - Redeem { condition_id, outcome, qty }
  - PauseMerges { reason }

Acceptance:
- Unit test: merge decision thresholds
- Integration test: mocked relayer/onchain call invoked
- Invariant: merged collateral increases available quoting budget
- Handles delayed resolution with retries and records pnl event

Dependencies:
- Inventory state from user WS (T5)
- Market condition_id from discovery (T3)

---

T16) Observability: metrics + health + structured logging
Owner files:
- src/ops/metrics.rs
- src/ops/health.rs
- src/ops/mod.rs
- src/ops/shutdown.rs

Goal:
- /healthz and /metrics endpoints
- add key metrics described in TECH_SPEC
- graceful shutdown cancels orders

Interfaces:
- start_http_servers(cfg, shared_state)
- shutdown signal integration

Acceptance:
- curl /healthz returns ok + state summary
- /metrics exposes prometheus format

Dependencies:
- T0, T2, T13

---

T17) Event logging + replay + stress harness (mandatory)
Owner files:
- src/persistence/event_log.rs
- src/persistence/replay.rs

Goal:
- Write JSONL event log of all significant events.
- Implement offline replay to reproduce quoting decisions deterministically.
- Implement replay-driven stress mode (1x, 5x) to validate throughput/latency/memory bounds.

Interfaces:
- EventLogger::log(event)
- ReplayRunner::run(log_path) -> outputs summary
- StressRunner::run(log_path, speed) -> outputs latency stats

Acceptance:
- replay produces same desired quotes given same inputs
- stress runner keeps invariants and bounded queues/memory
- CI includes a stress test on a recorded fixture
- log rotation works

Dependencies:
- T2, T11

------------------------------------------------------------
Suggested dependency graph (simplified)
------------------------------------------------------------

T0
 -> T1, T2
T2 -> T3, T4, T5, T6
T3 + T6 -> T7
T7 -> T9
T9 + T10 -> T11
T12 -> T13 -> T16
T12 -> T14
T3 + T5 -> T15
T11 -> T13
T2 + T11 -> T17
