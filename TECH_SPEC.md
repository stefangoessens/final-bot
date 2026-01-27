# TECH_SPEC.md
# Polymarket BTC 15m Market-Maker Bot (Rust) - TECH SPEC v1.1 (Alpha-first)

Last updated: 2026-01-26

This document is intentionally explicit and modular so AI coding agents can implement in parallel.

------------------------------------------------------------
0) Repo Layout (proposed)
------------------------------------------------------------

Rust crate: polymarket-mm-bot

src/
  main.rs
  config.rs
  error.rs
  time.rs

  state/
    mod.rs
    market_state.rs
    order_state.rs
    inventory.rs
    book.rs
    rtds_price.rs

  clients/
    mod.rs
    gamma.rs
    clob_rest.rs
    clob_ws_market.rs
    clob_ws_user.rs
    rtds_ws.rs

  alpha/
    mod.rs
    volatility.rs
    probability.rs
    toxicity.rs
    target_total.rs

  strategy/
    mod.rs
    quote_engine.rs
    reward_engine.rs
    risk.rs
    fee.rs

  execution/
    mod.rs
    order_manager.rs
    batch.rs
    heartbeats.rs

  ops/
    mod.rs
    logging.rs
    metrics.rs
    health.rs
    shutdown.rs

  persistence/
    mod.rs
    event_log.rs
    replay.rs (optional but recommended)

------------------------------------------------------------
1) External Dependencies
------------------------------------------------------------

Rust async runtime:
- tokio

HTTP:
- reqwest

WebSocket:
- tokio-tungstenite (or similar)

JSON:
- serde / serde_json

Observability:
- tracing
- tracing-subscriber
- prometheus (or opentelemetry if preferred)

(Optionally) Official Polymarket Rust SDK:
- Polymarket/rs-clob-client (if used), enable needed features (clob, gamma, heartbeats, ctf)
If not used, implement REST + signing manually (not recommended because feeRateBps signing requirement).

------------------------------------------------------------
2) Official Endpoints (hardcoded defaults, override via config)
------------------------------------------------------------

CLOB REST:
- https://clob.polymarket.com

CLOB WS:
- wss://ws-subscriptions-clob.polymarket.com/ws/market
- wss://ws-subscriptions-clob.polymarket.com/ws/user

Gamma:
- https://gamma-api.polymarket.com

RTDS:
- wss://ws-live-data.polymarket.com

Docs reference:
- Data Feeds: https://docs.polymarket.com/developers/market-makers/data-feeds
- RTDS Crypto Prices: https://docs.polymarket.com/developers/RTDS/RTDS-crypto-prices

------------------------------------------------------------
3) Core Data Models
------------------------------------------------------------

3.1 MarketIdentity
struct MarketIdentity {
  slug: String,
  interval_start_ts: i64,
  interval_end_ts: i64,        // derived from Gamma endDate
  condition_id: String,
  token_up: String,           // clobTokenId
  token_down: String,
}

3.2 TokenBookTop
struct TokenBookTop {
  best_bid: Option<f64>,
  best_ask: Option<f64>,
  tick_size: f64,
  last_update_ms: i64,
}

3.3 RTDSPrice
struct RTDSPrice {
  price: f64,
  ts_ms: i64,
}

3.4 MarketState
struct MarketState {
  identity: MarketIdentity,

  // Market data
  up_book: TokenBookTop,
  down_book: TokenBookTop,

  // Start price S0 for probability model
  start_btc_price: Option<f64>,
  start_price_ts_ms: Option<i64>,

  // RTDS latest prices
  rtds_primary: Option<RTDSPrice>, // btcusdt
  rtds_sanity: Option<RTDSPrice>,  // btc/usd

  // Alpha engine state
  alpha: AlphaState,

  // Orders + inventory
  orders: OrderState,
  inventory: InventoryState,

  // Trading control
  quoting_enabled: bool,
  cutoff_ts: i64,              // end - 60s
}

3.5 InventoryState
We need both position and cost basis.
struct InventorySide {
  shares: f64,
  notional_usdc: f64,          // sum(price*shares)
}
struct InventoryState {
  up: InventorySide,
  down: InventorySide,
  last_trade_ms: i64,
}
Methods:
- avg_cost_up() = up.notional_usdc / up.shares (if shares>0)
- avg_cost_down()

3.6 OrderState
We manage multiple levels.
struct LiveOrder {
  order_id: String,
  token_id: String,
  level: usize,               // 0..L-1
  price: f64,
  size: f64,                  // original size
  remaining: f64,             // from user ws
  status: OrderStatus,
  last_update_ms: i64,
}

struct OrderState {
  // key: (token_id, level)
  live: HashMap<(String, usize), LiveOrder>,
}

3.7 DesiredOrder
struct DesiredOrder {
  token_id: String,
  level: usize,
  price: f64,
  size: f64,
  post_only: bool,
  tif: TimeInForce,           // GTC
}

------------------------------------------------------------
4) Concurrency / Runtime Architecture
------------------------------------------------------------

Use an actor-like architecture with tokio tasks and channels.

4.1 Tasks
- Task A: MarketDiscoveryLoop
  - periodically computes current + next slug and fetches Gamma metadata.
  - publishes MarketIdentity updates.

- Task B: CLOBMarketWsLoop
  - maintains WS connection to /ws/market
  - subscribes to token_ids for all active markets
  - updates TokenBookTop and tick size
  - publishes MarketDataEvent to StateManager

- Task C: CLOBUserWsLoop
  - maintains WS connection to /ws/user (auth)
  - consumes order/trade events
  - updates OrderState + InventoryState
  - publishes FillEvent / OrderEvent

- Task D: RTDSWsLoop
  - connects to RTDS, subscribes to btcusdt and btc/usd
  - updates RTDS prices
  - publishes RTDSEvent

- Task E: StateManager
  - single-threaded owner of canonical in-memory state
  - applies events from A/B/C/D
  - triggers QuoteTick events to StrategyEngine

- Task F: StrategyEngine
  - on QuoteTick, reads MarketState snapshot
  - runs AlphaEngine + QuoteEngine -> DesiredOrders
  - sends DesiredOrders to OrderManager

- Task G: OrderManager
  - diffs desired vs live orders
  - executes cancel/post (batch) via CLOB REST client
  - updates state via OrderSubmitted/OrderCanceled events (or rely on user ws confirmations)

- Task H: Resolver/Redeemer
  - monitors ended markets, waits for resolution, redeems

- Task I: Metrics/Health/HTTP
  - exports /metrics and /healthz

### InventoryEngine (new core module)
Add a dedicated InventoryEngine actor responsible for:
- monitoring pos_up/pos_down per market
- deciding when to MERGE full sets to collateral (capital velocity)
- deciding when to REDEEM post-resolution
- managing wallet mode differences:
  - RELAYER/PROXY: gasless, can merge more frequently
  - EOA: merge thresholding and max ops/min to cap gas burn

Interfaces:
- InventoryEngine::tick(now_ms, market_states_snapshot) -> Vec<InventoryAction>
- InventoryAction:
  - Merge { condition_id, qty_sets }
  - Redeem { condition_id, outcome, qty }
  - PauseMerges { reason }

4.2 State ownership rule (important)
- ONLY StateManager mutates MarketState.
- All other tasks send events to StateManager.
- StrategyEngine uses cloned snapshots (immutable) for deterministic decisions.

------------------------------------------------------------
5) WebSocket Message Handling
------------------------------------------------------------

5.1 CLOB Market WS (Market Channel)
- Implement subscribe messages per docs.
- Support message types:
  - best_bid_ask
  - tick_size_change
  - book / price_change (choose one approach):
    Option 1 (simpler): subscribe to "best_bid_ask" + "tick_size_change" only.
      - Sufficient for top-of-book quoting.
    Option 2 (stronger): maintain local L2 book with book snapshots and price_change deltas.

Given speed/edge requirement, implement Option 2 if feasible.

Important:
- There was a schema migration for price_change (see migration guide). Use docs and handle new object schema.

5.2 CLOB User WS
- Subscribe with auth payload (apiKey/secret/passphrase) and optionally filter markets.
- Parse and emit:
  - Trade events: update inventory + cost basis
  - Order events: update order remaining + status

Staleness:
- If user WS disconnected, set a global flag that pauses quoting.

5.3 RTDS WS
Subscribe messages (from RTDS docs):
- Binance:
  {
    "action":"subscribe",
    "subscriptions":[{"topic":"crypto_prices","type":"update","filters":"btcusdt"}]
  }
- Chainlink:
  {
    "action":"subscribe",
    "subscriptions":[{"topic":"crypto_prices_chainlink","type":"*","filters":"{\"symbol\":\"btc/usd\"}"}]
  }
Message schema:
{
  "topic": "...",
  "type": "...",
  "timestamp": <ms>,
  "payload": {"symbol": "...", "timestamp": <ms>, "value": <number>}
}

### Oracle-aligned alpha (Chainlink primary)
- AlphaEngine must compute S0 and St from Chainlink BTC/USD (RTDS chainlink source) when available.
- Binance feed is used for fast-move detection, not for settlement truth.
- Regime decisions:
  - Chainlink stale => hard pause
  - Binance stale => size reduction only
------------------------------------------------------------
6) AlphaEngine Implementation Details
------------------------------------------------------------
alpha/volatility.rs
- Maintain a rolling EWMA estimator:
  - var_per_s (f64)
  - drift_per_s (f64)
  - last_price (f64)
  - last_ts_ms (i64)

Expose:
- fn update(price: f64, ts_ms: i64) -> AlphaInputsUpdate
- fn var_per_s() -> f64
- fn drift_per_s() -> f64

alpha/probability.rs
- fn compute_q_up(S0: f64, St: f64, tau_s: f64, mu: f64, var_per_s: f64) -> f64
- Implement Phi via erf approximation or statrs crate.

alpha/toxicity.rs
- Detect FAST_MOVE using return over window and/or var threshold.
- Detect STALE_ORACLE if now - chainlink_ts > chainlink_stale_ms (hard pause)
- Detect STALE_BINANCE if now - binance_ts > binance_stale_ms (soft: size down)
- Detect ORACLE_DISAGREE as warning only (no halt by itself)

alpha/target_total.rs
- target_total = clamp(base - penalties, min, max)

Alpha output:
struct AlphaOutput {
  regime: Regime,
  q_up: f64,
  cap_up: f64,
  cap_down: f64,
  target_total: f64,
  size_scalar: f64,     // reduce size under toxicity
}

------------------------------------------------------------
7) QuoteEngine Implementation Details
------------------------------------------------------------

strategy/quote_engine.rs
Input:
- MarketState snapshot
- Config

Output:
- Vec<DesiredOrder> for Up and Down (L levels each) OR empty if quoting disabled.

Steps:
1) If now >= cutoff_ts: return empty and request cancels.
2) If regime in {STALE_ORACLE}: return empty and request cancels.
3) Determine p_up0, p_down0 using:
   - competitive comp_i from best_bid/best_ask/tick
   - model caps cap_i
   - combined cap enforcement
4) Ladder levels:
   - for l in 0..L: compute price and size
5) Inventory skew modifiers:
   - adjust sizes
   - possibly drop excess side level0 in moderate/severe skew
6) Return DesiredOrders.

strategy/fee.rs
Implement taker fee function:
fee_usdc(shares, price) = shares * price * 0.25 * (price*(1-price))^2
Also compute fee_per_share.
strategy/risk.rs
- Determine if taker completion allowed and compute max acceptable taker price.
- Provide escalation based on time_to_cancel and unpaired size.

strategy/reward_engine.rs
- If enable_liquidity_rewards_chasing:
  - periodically call /orders-scoring for open orders
  - if not scoring, annotate desired changes (size/price) but do not violate risk constraints.

------------------------------------------------------------
8) Execution / OrderManager
------------------------------------------------------------

### Completion orders use FOK/FAK with explicit price cap
OrderManager must support:
- Maker quoting: GTC limit, postOnly=true
- Completion/taker: FOK or FAK “market order” behavior with explicit limit price P_max

This relies on CLOB order types:
- FOK: fill entirely immediately or cancel
- FAK: fill available immediately, cancel remainder

Implementation guidance:
- prefer SDK method createAndPostMarketOrder (FOK/FAK) if available,
  else use REST orderType=FOK/FAK with signed order payload and explicit price.

### Heartbeats mandatory
- Heartbeats must be enabled and supervised by a dedicated task.
- On heartbeat failure beyond grace:
  - emit global CancelAll
  - pause quoting until restored

### Replay + stress harness is mandatory
- Persist a unified event log (market ws + user ws + RTDS + decisions).
- Provide:
  - deterministic replay mode (feeds events into StateManager and StrategyEngine)
  - stress mode (replay at N-times speed, measure queue sizes, memory, and latency)

execution/order_manager.rs
Inputs:
- DesiredOrders for a market
- Current live orders from state (or via snapshot)
- Rate limiter

Algorithm:
1) Build diff:
   - For each desired (token,level):
     - If no live order -> create
     - If live exists but needs update -> cancel+create
   - For each live order not in desired -> cancel
2) Apply churn rules:
   - do not update if within min_update_interval and change is small
3) Use batch endpoints:
   - cancel_orders([...])
   - post_orders([...])
4) Ensure feeRateBps is fetched and included before signing (or use official client that does it).

Important:
- Avoid "cancel+repost" loop on tiny book moves.

------------------------------------------------------------
9) Redeemer
------------------------------------------------------------

resolver loop:
- For each market ended:
  - Cancel any remaining orders
  - Wait for resolution (Gamma status or WS market_resolved)
  - Redeem winning tokens via CTF operations
  - Emit PnL event

------------------------------------------------------------
10) Observability / Ops
------------------------------------------------------------

Logging:
- tracing with JSON logs
- include: market_slug, condition_id, token_id, level, order_id, price, size, regime, q_up, target_total

Metrics (prometheus):
- ws_market_connected (gauge)
- ws_user_connected (gauge)
- rtds_connected (gauge)
- ws_message_lag_ms (histogram)
- order_submit_latency_ms (histogram)
- order_rejects_total
- fills_total
- unpaired_shares (gauge per market)
- pnl_realized_usdc (counter/gauge)
- fee_paid_usdc (counter)
- rebate_estimated_usdc (counter)

Health endpoints:
- /healthz: returns ok if all feeds healthy OR if halted with explicit reason.
- /metrics: prometheus

Deployment:
- Docker + ECS (or EC2 systemd)
- AWS eu-west-1
- Secrets from AWS Secrets Manager

------------------------------------------------------------
11) Security / Compliance
------------------------------------------------------------

- Do not bypass geoblocks.
- Never log secrets (private key, api secret, passphrase).
- If georestricted detected: halt and cancel orders.

------------------------------------------------------------
12) Testing Strategy (must exist)
------------------------------------------------------------

Unit tests:
- fee curve calculation matches docs table points
- probability model sanity checks
- tick rounding
- combined cap enforcement

Integration tests:
- dry-run mode (no posting) that connects to WS/RTDS and generates quotes
- optional sandbox if available

Replay tests:
- given recorded event log, re-run QuoteEngine and assert deterministic outputs.
