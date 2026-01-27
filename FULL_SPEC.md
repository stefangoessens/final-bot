# FULL_SPEC.md
# Polymarket BTC 15m Market-Maker Bot (Rust) - FULL SPEC v1.1 (Alpha-first)

Last updated: 2026-01-26
Region/Deployment target: AWS eu-west-1

This spec is a fully implementable blueprint for AI coding agents.
Core product from day 1: strong execution + an explicit edge ("alpha gating") so we are not bot #500.

--------------------------------------------------------------------
0) Official Docs (must be referenced during implementation)
--------------------------------------------------------------------

Core APIs / Quickstart:
- Developer Quickstart: https://docs.polymarket.com/quickstart/overview
- Endpoints: https://docs.polymarket.com/quickstart/reference/endpoints
- API Rate Limits: https://docs.polymarket.com/quickstart/introduction/rate-limits

Gamma (market discovery):
- Gamma Overview: https://docs.polymarket.com/developers/gamma-markets-api/overview
- Fetch Markets Guide: https://docs.polymarket.com/developers/gamma-markets-api/fetch-markets-guide
- Gamma Structure: https://docs.polymarket.com/developers/gamma-markets-api/gamma-structure

CLOB Trading:
- CLOB Introduction: https://docs.polymarket.com/developers/CLOB/introduction
- Authentication: https://docs.polymarket.com/developers/CLOB/authentication
- Orders Overview: https://docs.polymarket.com/developers/CLOB/orders/orders
- Create Order: https://docs.polymarket.com/developers/CLOB/orders/create-order
- Batch Orders: https://docs.polymarket.com/developers/CLOB/orders/create-order (see batching section in nav)
- Get Active Orders: https://docs.polymarket.com/developers/CLOB/orders/get-active-order
- Cancel Orders: https://docs.polymarket.com/developers/CLOB/orders/cancel-orders
- Order Scoring (Liquidity Rewards eligibility): https://docs.polymarket.com/developers/CLOB/orders/check-scoring
- Historical Timeseries Data (optional, for research/replay): https://docs.polymarket.com/developers/CLOB/timeseries

CLOB WebSocket:
- WSS Overview: https://docs.polymarket.com/developers/CLOB/websocket/wss-overview
- Market Channel: https://docs.polymarket.com/developers/CLOB/websocket/market-channel
- User Channel: https://docs.polymarket.com/developers/CLOB/websocket/user-channel
- WSS Auth: https://docs.polymarket.com/developers/CLOB/websocket/wss-auth
- Data Feeds (includes exact ws URLs and examples): https://docs.polymarket.com/developers/market-makers/data-feeds

RTDS (low-latency crypto price feed):
- RTDS Overview: https://docs.polymarket.com/developers/RTDS/RTDS-overview
- RTDS Crypto Prices (subscription + message schema): https://docs.polymarket.com/developers/RTDS/RTDS-crypto-prices

Fees + Maker Rebates (15m crypto markets):
- Maker Rebates Program (user): https://docs.polymarket.com/polymarket-learn/trading/maker-rebates-program
- Maker Rebates Program (dev guide, feeRateBps signing): https://docs.polymarket.com/developers/market-makers/maker-rebates-program
- Changelog (taker fees + maker rebates on 15m crypto): https://docs.polymarket.com/changelog/changelog

Liquidity Rewards (separate program from maker rebates):
- Liquidity Rewards (dev): https://docs.polymarket.com/developers/market-makers/liquidity-rewards
- Liquidity Rewards (user): https://docs.polymarket.com/polymarket-learn/trading/liquidity-rewards

Inventory Management (MM guidance):
- Inventory Management: https://docs.polymarket.com/developers/market-makers/inventory

--------------------------------------------------------------------
1) Purpose / Scope
--------------------------------------------------------------------

We build a Rust/Tokio bot that trades ONLY:
- Polymarket Bitcoin "Up/Down 15-minute" markets (CLOB)

The bot continuously maintains maker bids on BOTH outcomes (Up + Down) with:
- A combined top-level buy price cap: p_up + p_down <= target_total
- target_total is dynamic, but constrained: [target_total_min, target_total_max]
- Baseline target_total ranges ~0.98 to 0.99 (configurable), but may reduce under toxicity.

We always trade two markets:
- Current 15m market
- Next 15m market ("warm" to reduce downtime at roll)

The bot must stop quoting and cancel orders 60 seconds before market end.

Core edge (moat) MUST exist from start:
1) Speed + reliability: local L2 book, incremental updates, minimal churn, batch order ops.
2) Alpha gating: do not blindly quote; use RTDS BTC feed + volatility/toxicity model to avoid getting picked off.
3) Reward optimization: maker rebates + liquidity rewards scoring awareness.
4) Data flywheel: record full state (book + RTDS + orders + fills) for continuous improvements.

### How we make money (explicit)
Primary deterministic mechanism:
1) Acquire paired inventory (Up + Down) at total cost < 1.0 (set-completion edge).
2) Immediately MERGE paired sets back into collateral to recycle capital and compound ROI.

Why this is not commodity:
- Many bots “hold to resolution” and suffer capital lock; merging converts edge into velocity.
- Our oracle alignment uses Chainlink (settlement truth) and uses Binance only for fast-move detection.
- Our completion trades have explicit worst-price caps (FOK/FAK), reducing tail-loss from thin books.
- Incentives (maker rebates, liquidity rewards) are treated as additive upside, never the sole profit source.

Non-negotiable operational requirement:
- We log everything for replay and calibration (see performance & data sections).

Non-goals:
- Trading non-BTC-15m markets.
- Multi-venue hedging (CEX hedging) unless explicitly added later.
- Complex ML training online. But we DO implement a deterministic fair-probability model + toxicity filters.

--------------------------------------------------------------------
2) Definitions / Market Model
--------------------------------------------------------------------

Each 15m market has two complementary outcome tokens:
- "Up" token pays 1 USDC if BTC_up condition occurs at resolution, else 0.
- "Down" token pays 1 USDC if BTC_down condition occurs at resolution, else 0.
(Exact oracle definition is external; we treat payout as binary complementary.)

### Settlement Oracle Source (Required)
- The resolution source for BTC “Up/Down 15-minute” markets is the Chainlink BTC/USD data stream (settlement truth).
- Therefore:
  - The bot’s probability model and S0/S_end comparisons MUST be computed on Chainlink BTC/USD whenever available.
  - Binance (RTDS `crypto_prices`) is used only as a leading indicator for toxicity and fast-move detection, not as truth.
- If Chainlink feed is stale:
  - bot must treat the market as “degraded” and reduce exposure or pause (configurable), because settlement truth is unavailable.

Operational note:
- We still subscribe to both RTDS sources:
  - Chainlink: crypto_prices_chainlink (BTC/USD)
  - Binance: crypto_prices (BTCUSDT)

If we acquire matched pairs (1 Up + 1 Down), payout at resolution is deterministic 1 USDC per pair.

Core deterministic edge:
edge_per_pair = 1.0 - (avg_cost_up + avg_cost_down) - taker_fees_paid - other_costs

Primary risk source:
- Leg risk: fills arrive on one side more than the other -> directional exposure.

Important:
- On 15m crypto markets, taker fees exist and maker rebates exist.
- Fees vary with price via a curve; see maker rebates program doc.

--------------------------------------------------------------------
3) System Components (high level)
--------------------------------------------------------------------

(1) MarketDiscovery
- Computes current + next slugs every loop (or on 15m boundary), fetches Gamma metadata.
- Validates market is tradable (active/acceptingOrders/not restricted).

(2) Feeds
- CLOB market websocket: orderbook + tick size updates for token IDs.
- CLOB user websocket: order/trade events for our account.
- RTDS websocket: BTC price feed (Binance source + Chainlink source).

(3) State Store (in-memory, deterministic)
- MarketState per tracked market:
  - condition_id
  - end_date_utc
  - up_token_id / down_token_id
  - tick_size per token
  - L2 orderbook snapshots (best bid/ask + optional depth)
  - open orders tracked (per token per level)
  - positions + cost basis per token
  - staleness timestamps for each feed

(4) AlphaEngine (core edge)
- Computes:
  - q_up: model probability Up at settlement
  - volatility estimate (sigma_per_s)
  - toxicity score (fast moves, oracle health, book instability)
- Outputs:
  - dynamic target_total
  - price caps per side (fair value gating)
  - size multipliers (scale down under toxicity)

(5) QuoteEngine
- Produces desired orders (multi-level ladder) for each token:
  - price per level
  - size per level
  - post_only flag
- Enforces invariants:
  - top-level p_up0 + p_down0 <= target_total
  - post_only: do not cross best ask
  - tick size rounding
  - min size
  - inventory-aware skew bias and escalation rules

(6) Execution / OrderManager
- Compares desired orders vs live open orders, decides cancel/replace with debounce.
- Uses batching endpoints where possible.
- Maintains heartbeats / safety cancellation on disconnect.

(7) Resolver/Redeemer
- After market end: wait for resolution, then redeem winnings.
- Retries, alerts on stuck resolution.

(8) Observability + Data Capture
- Structured logs + metrics + event recording for replay analysis.

--------------------------------------------------------------------
4) Market Discovery & Scheduling
--------------------------------------------------------------------

4.1 Slug computation
Let now_utc be current UTC Unix seconds.

interval_start = floor(now_utc / 900) * 900
current_slug = "btc-updown-15m-{interval_start}"
next_slug    = "btc-updown-15m-{interval_start + 900}"

4.2 Gamma fetch
For each slug:
- Fetch market metadata from Gamma (exact endpoint shape may evolve; use Gamma docs).
We need at minimum:
- conditionId (market id used in user ws filter)
- clobTokenIds (Up/Down token IDs)
- endDate (UTC timestamp)
- flags: active / closed / acceptingOrders / restricted

If computed slug does not exist yet:
- retry with exponential backoff up to max_wait_next_market_seconds
- also attempt Gamma search (prefix query "btc-updown-15m-") to locate nearest.

4.3 Tradability rules
A market is tradable if ALL true:
- acceptingOrders == true
- restricted == false
- active == true
- now_utc < endDate - cutoff_seconds (cutoff_seconds=60)

If not tradable:
- do not quote, cancel existing orders for that market, and keep watching for roll.

4.4 Rolling rules
At every 15m boundary:
- current becomes previous next (or recomputed)
- next becomes new next slug
- Ensure we keep next warm with WS subscriptions and at least one quoting cycle (unless not yet live).

--------------------------------------------------------------------
5) Data Feeds - Requirements & Edge Cases
--------------------------------------------------------------------

5.1 CLOB market WebSocket
Use official endpoints:
- wss://ws-subscriptions-clob.polymarket.com/ws/market
See Data Feeds docs for examples.

We must subscribe for each token id we care about.
Minimum messages to support:
- best_bid_ask
- book (or price_change + local book)
- tick_size_change
- last_trade_price (optional but useful)

We must maintain per-token:
- best_bid, best_ask
- spread
- tick_size
- last_update_ts

Staleness:
- If no message for token > market_ws_stale_ms, mark token market data stale.
- When stale: cancel orders for that token and pause quoting until fresh.

Sequence numbers / missed updates:
- If using price_change deltas, MUST track sequence numbers as per docs and resync book when gap detected.
- If implementing "book" snapshots periodically, still track staleness.

5.2 CLOB user WebSocket
Endpoint:
- wss://ws-subscriptions-clob.polymarket.com/ws/user
Authenticate with apiKey/secret/passphrase in subscribe message (see Data Feeds + WSS Auth docs).

Must process:
- order events: placed, open, partial fill, filled, canceled, rejected
- trade events: fills with price, size, order_id, side, timestamp

We treat the user WS as the source of truth for fills and live remaining order sizes.

If user WS disconnects:
- pause quoting (cannot manage inventory safely)
- optionally cancel all open orders (config: cancel_on_user_ws_disconnect)

5.3 RTDS - BTC Price Feed (required for alpha gating)
Endpoint:
- wss://ws-live-data.polymarket.com
Subscribe to:
(A) Binance source topic "crypto_prices" type "update" filter "btcusdt"
(B) Chainlink source topic "crypto_prices_chainlink" type "*" filter {"symbol":"btc/usd"}

Use RTDS Crypto Prices doc schemas.

We maintain:
- P_chainlink: latest Chainlink BTC/USD price (settlement truth)
- P_binance:   latest Binance BTCUSDT price (leading indicator only)
- last update timestamps for both

### RTDS Regimes (Replaces naive “divergence halt”)
We define regimes based on (A) feed health and (B) fast move conditions.

Inputs (RTDS):
- P_chainlink: latest Chainlink BTC/USD price (crypto_prices_chainlink)
- P_binance:   latest Binance BTCUSDT price (crypto_prices)
- ts_chainlink, ts_binance
- now_ms

Config thresholds:
- rtds_chainlink_stale_ms
- rtds_binance_stale_ms
- fast_move_window_ms
- fast_move_threshold_bps
- oracle_disagree_threshold_bps (large; used only as a “warning” unless paired with staleness)

Regimes:
1) STALE_ORACLE (hard)
- if now_ms - ts_chainlink > rtds_chainlink_stale_ms:
  - Cancel all open orders for affected markets
  - Pause quoting until Chainlink feed is fresh again
Reason: settlement oracle is unknown.

2) STALE_BINANCE (soft)
- if now_ms - ts_binance > rtds_binance_stale_ms:
  - Reduce sizes (size_scalar < 1)
  - Keep quoting if Chainlink is healthy
Reason: lose early-warning, but settlement oracle still available.

3) FAST_MOVE
- computed from Binance returns (preferred) OR Chainlink returns if Binance stale:
  - if abs(return over fast_move_window_ms) > fast_move_threshold_bps:
    - Reduce sizes (size_scalar)
    - Reduce target_total (require more edge)
    - Optionally reduce ladder_levels (less exposure)
Reason: avoid toxic fills; do not necessarily halt.

4) ORACLE_DISAGREE (warning, not automatic halt)
- if abs(P_binance_to_usd - P_chainlink)/mid > oracle_disagree_threshold_bps:
  - Mark WARNING
  - No halt unless ALSO Chainlink is stale or we detect WS staleness.
Reason: divergence is common; settlement is Chainlink.

--------------------------------------------------------------------
6) AlphaEngine (Core Edge / Not Optional)
--------------------------------------------------------------------

AlphaEngine provides two things:
(1) Fair-probability split for Up vs Down (q_up vs q_down)
(2) Toxicity-aware dynamic edge requirement and sizing.

6.1 Inputs
Per market:
- start_time = interval_start of that market (derived from slug)
- end_time   = market endDate from Gamma
From RTDS:
- btc price time series (primary + sanity)
From CLOB:
- midpoint estimate from books (optional)
- spread and depth
From internal state:
- inventory skew, time_to_cancel, time_to_end

6.2 Track start price S0 for each market
At market start_time:
- S0 = first Chainlink BTC/USD price seen at or after start_time (use closest tick).
- Store S0 in MarketState and never change it unless missing (then set when first available).
This is required to compute probability that end price > start price.

6.3 Volatility estimator (per second)
We compute log returns r_i = ln(P_i / P_{i-1}) from Chainlink BTC/USD updates (settlement oracle).
For each update:
- dt = max(1e-3, (t_i - t_{i-1}) in seconds)
- x = r_i / sqrt(dt)
Maintain EWMA variance:
- var_per_s = lambda * var_per_s + (1 - lambda) * x^2
Where lambda = exp(-ln(2) * dt / vol_halflife_s)

Then predicted variance over horizon tau_s seconds:
- var_horizon = var_per_s * tau_s
- sigma_horizon = sqrt(var_horizon)

6.4 Drift estimator (optional but included; alpha, not fancy ML)
Maintain EWMA drift_per_s over normalized returns:
- drift_per_s = lambda_d * drift_per_s + (1 - lambda_d) * (r_i / dt)
Clamp drift_per_s to +/- drift_clamp_per_s to avoid insanity.

6.5 Probability model q_up
Let:
- S0 = start price for market
- St = current Chainlink BTC/USD price (settlement oracle)
- tau = max(1e-3, end_time - now) in seconds
Let mu = drift_per_s (clamped)
Let sigma2_per_s = var_per_s (>= min_var)

Assume log price evolves as:
ln(S_end) ~ Normal( ln(St) + mu * tau, sigma2_per_s * tau )

Condition for Up: S_end > S0  <=> ln(S_end) > ln(S0)

Compute z:
z = (ln(S0) - ln(St) - mu * tau) / sqrt(sigma2_per_s * tau)

Then:
q_up = 1 - Phi(z)
q_down = 1 - q_up

Implementation notes:
- If S0 missing: q_up = 0.5 (fallback)
- If sigma2_per_s very small: treat q_up as step function based on St vs S0 with small epsilon band.

6.6 Toxicity score and regimes
We define a discrete regime:
- NORMAL
- FAST_MOVE
- STALE_ORACLE
- STALE_BINANCE

FAST_MOVE triggers if:
- abs(btc_return_over_fast_window) > fast_move_threshold (e.g. 10 bps) OR
- var_per_s > var_threshold

STALE_ORACLE triggers if Chainlink BTC/USD is stale (Section 5.3 RTDS regimes).
STALE_BINANCE triggers if Binance is stale (Section 5.3 RTDS regimes).

ORACLE_DISAGREE is a warning only (does not halt by itself).

When regime != NORMAL:
- cancel orders (immediate) if STALE_ORACLE.
- reduce target_total and size aggressively if FAST_MOVE.

6.7 Dynamic target_total (edge requirement)
Base:
- target_total_base (e.g. 0.985)
Constraints:
- target_total_min (e.g. 0.97)
- target_total_max (e.g. 0.99)

Compute penalties:
- vol_penalty = k_vol * clamp(var_per_s / var_ref, 0..1)
- spread_penalty = k_spread * clamp(avg_spread / spread_ref, 0..1)
- time_penalty = k_time * clamp(1 - (time_to_cancel / time_scale), 0..1)
- fast_move_penalty = if FAST_MOVE then k_fast else 0

target_total = clamp(
  target_total_base - vol_penalty - spread_penalty - time_penalty - fast_move_penalty,
  target_total_min,
  target_total_max
)

Intuition:
- When market is toxic / near close, we require more edge (lower target_total) and/or quote smaller.

6.8 Fair value caps per side (prevents bot-500 behavior)
We want each side price to be <= "fair price * target_total" (scaled to preserve deterministic edge).
Define:
- cap_up   = q_up * target_total
- cap_down = q_down * target_total

These are "model caps": we never bid above these unless inventory emergency rules apply.

--------------------------------------------------------------------
7) QuoteEngine (Pricing + Multi-level + Inventory + Rewards)
--------------------------------------------------------------------

7.1 Quote objectives
- Maintain bids for Up and Down continuously (until cutoff)
- Prefer maker (post_only=true)
- Keep top-level combined cap: p_up0 + p_down0 <= target_total
- Be competitive, but never exceed model caps in normal mode
- Minimize churn to keep queue position
- Keep inventory paired as much as possible
- Keep orders eligible for incentives when possible (maker rebates come from fills; liquidity rewards need scoring)

7.2 Inputs
For each token:
- best_bid, best_ask, tick_size
- optional depth and midpoint
- model caps cap_up/cap_down
- inventory state: pos_up, pos_down, avg_cost_up, avg_cost_down
- time_to_cancel
- regime (NORMAL/FAST_MOVE/etc)
- reward settings (optional)

7.3 Competitive bid candidate
For token i:
- tick = tick_size_i
- comp = best_bid + improve_ticks * tick
- Ensure post-only:
  comp <= best_ask - min_ticks_from_ask * tick
If best_ask missing (no asks):
- use comp = best_bid (do not increase) and reduce size.

7.4 Price selection (normal)
For token i:
- p0_raw = min(comp, cap_i)
- p0 = floor_to_tick(p0_raw, tick)

After both computed:
- If p_up0 + p_down0 > target_total (due to rounding):
  - Reduce the side with larger (p0 - best_bid) first by 1 tick until sum <= target_total.
  - If still > target_total, reduce both alternately until satisfied.

7.5 Multi-level ladder (core from start)
We place L levels per token (config ladder_levels >= 1).
Level 0 is top.
For level l:
- price_i_l = max(min_price, p_i_0 - l * ladder_step_ticks * tick)
- size_i_l  = base_size_shares * size_multiplier(level=l) * inventory_multiplier_i

size_multiplier(level):
- default: size_decay^l (e.g. 0.5^l)

Invariant:
- Only enforce combined cap on level 0.
- Since deeper levels are cheaper, combined cap holds for any mix.

7.6 Inventory-aware sizing and skew bias
Let:
- pos_up, pos_down (shares)
- unpaired = abs(pos_up - pos_down)
- missing_side = Up if pos_up < pos_down else Down
- excess_side = the other

Define thresholds:
- skew_mild
- skew_moderate
- skew_severe

Actions:
(A) unpaired <= skew_mild:
- Quote both sides normally.

(B) skew_mild < unpaired <= skew_moderate:
- Increase missing side size: missing_size_mult = 1 + skew_size_k * (unpaired / skew_moderate)
- Decrease excess side size: excess_size_mult = 1 / missing_size_mult
- Optionally increase improve_ticks on missing side by +1 (still within cap).

(C) skew_moderate < unpaired <= skew_severe:
- Missing side:
  - improve_ticks_missing = base_improve_ticks + skew_improve_extra (up to max)
  - allow cap override by small epsilon only if completion EV still positive (see 7.8)
- Excess side:
  - remove level 0 order (keep only deeper level(s) or cancel all on excess side)
  - objective: stop increasing exposure in the wrong direction

(D) unpaired > skew_severe OR time_to_cancel < emergency_window_s:
- Emergency completion mode:
  - Post-only maker: place missing side bid at best_ask - 1 tick (if allowed) but still not above cap_emergency if configured.
  - If still not filling and time_to_cancel < taker_window_s:
    allow taker completion (Section 7.8).

7.7 Churn minimization (queue position edge)
We ONLY cancel/replace an existing order if:
- abs(new_price - old_price) >= reprice_min_ticks * tick
OR
- abs(new_size - old_size) / old_size >= resize_min_pct
OR
- order is no longer post-only safe (would cross due to ask moving down)
OR
- order no longer scoring (if scoring required) and we can adjust without breaking risk constraints.

Also enforce:
- min_update_interval_ms per token per level

7.8 Taker completion logic (allowed but controlled; alpha-based)
### Completion Trades MUST use explicit worst-price + FOK/FAK
When we need to complete pairs quickly (inventory risk), taker-mode is allowed ONLY under this strict model:

Inputs:
- desired completion qty Q (shares)
- computed maximum acceptable price P_max (based on EV after fees and/or emergency loss cap)
- current top-of-book ask P_ask

Rule:
- We place an immediate order with a hard price cap P_max, using:
  - FOK if we require full completion immediately (all-or-nothing)
  - FAK if partial completion is acceptable

Implementation:
- Use OrderType.FOK or OrderType.FAK (per CLOB docs) with explicit limit price = P_max.
- Never submit an unbounded “marketable” order.
- If P_ask > P_max: do NOT take; fall back to maker-aggressive completion (bid at best_ask - tick, still post-only=false only if explicitly allowed).

Fee handling:
- Before any completion trade, compute fee estimate from documented fee-equivalent formula and/or observed effective fee.
- Enforce max loss per completion burst (config).

We compute taker fee using official formula (also used for fee-curve weighted rebates):
fee_usdc = shares * price * 0.25 * (price * (1 - price))^2
(see Maker Rebates Program docs)

We only do taker completion when:
- In emergency windows (near cutoff) AND
- Completion profit is >= completion_min_profit_per_share (dynamic; can drop toward 0 near close) OR
- Risk policy requires flattening even at small loss (explicit config allow_negative_completion=false by default)

Completion profit per share when we have excess side already:
Example: pos_up > pos_down, we need to buy Down to pair:
- avg_cost_up = filled_notional_up / filled_shares_up
- taker_price_down = current best_ask_down (used only if <= P_max)
- fee = fee_usdc(shares=1, price=taker_price_down) (scale by shares)

profit_per_share = 1.0 - avg_cost_up - taker_price_down - fee_per_share

Only proceed if:
profit_per_share >= completion_min_profit_per_share

If we cannot complete profitably:
- Prefer maker aggressive completion for a few seconds
- If still not possible, keep directional exposure but size down drastically and stop quoting excess side.

7.9 Incentive optimization (maker rebates + liquidity rewards)
Maker rebates:
- We maximize maker fills while controlling toxicity.
- During fee-curve weighted periods, rebate weight is proportional to "fee_equivalent" for maker fills:
fee_equivalent = shares * price * 0.25 * (price * (1 - price))^2
This pushes value toward mid prices (~0.5). Our strategy naturally quotes near ~0.49 each side; good.

Liquidity rewards:
- If liquidity rewards are active for the market, we want our orders to be "scoring".
- Use CLOB endpoint(s) to check scoring:
  - GET /order-scoring?order_id=...
  - POST /orders-scoring
If enable_liquidity_rewards_chasing == true:
- Maintain at least one scoring order per book (Up and Down) when it does not violate:
  - target_total
  - fair value caps
  - inventory safety
- If order not scoring, attempt:
  - increase size to meet min shares
  - move closer to midpoint (within cap constraints)

Important:
- Reward chasing must NEVER override safety constraints during FAST_MOVE / STALE_ORACLE regimes.

--------------------------------------------------------------------
8) Execution & Order Management
--------------------------------------------------------------------

8.1 Order types
Default:
- Side: BUY
- OrderType: GTC
- post_only: true

Completion/taker:
- Use OrderType.FOK or OrderType.FAK with explicit limit price cap (P_max).

8.2 Fee handling for signing
For fee-enabled markets, orders must include feeRateBps in signed payload.
Official clients fetch this via:
- GET https://clob.polymarket.com/fee-rate?token_id={token_id}
and include it in the order object before signing (dev Maker Rebates Program doc).
We MUST fetch dynamically and cache with TTL.

Note:
- feeRateBps is required for signature validation; it is not the same as the fee curve computed in 7.8.
- Use the official doc instructions.

8.3 Batch operations
Prefer:
- post_orders for multiple orders
- cancel_orders for multiple orders
to reduce latency and API load.

8.4 Heartbeats / safety cancellation
### Heartbeats are mandatory
- The bot MUST enable the official CLOB heartbeat / cancel-on-disconnect mechanism.
- If heartbeats are not active or fail for > heartbeat_grace_ms:
  - immediately cancel all open orders
  - pause quoting until heartbeats are restored
- “Watchdog-based” cancellation is not sufficient as the primary protection.

Config keys:
- heartbeats_enabled (bool, default true)
- heartbeat_interval_ms (e.g., 1000)
- heartbeat_grace_ms (e.g., 2500)

8.5 Rate limiting
Respect official rate limits; implement:
- token-bucket limiter per endpoint group
- batching
- debounce logic

8.6 Shutdown
On SIGINT/SIGTERM:
- cancel all open orders (both tracked markets)
- flush event logs
- exit cleanly

--------------------------------------------------------------------
9) Expiry Handling
--------------------------------------------------------------------

For each market:
cancel_time = endDate - cutoff_seconds (cutoff_seconds=60)

At cancel_time:
- cancel all orders in that market
- mark market as "no_new_orders"
- continue quoting the next market normally

After endDate:
- do not trade
- wait for resolution and redeem

--------------------------------------------------------------------
10) Resolution & Redemption
--------------------------------------------------------------------

### Capital Velocity: MERGE full sets (Required)
We MUST implement a merge loop to recycle collateral during the market, not only after resolution.

Definition:
- full_sets = min(pos_up_shares, pos_down_shares)

Behavior:
- If full_sets >= merge_min_sets AND merge_enabled=true:
  - Merge `merge_qty = floor(full_sets / merge_batch_sets) * merge_batch_sets`
  - After merge, reduce both pos_up and pos_down by merge_qty, and increase collateral balance by merge_qty (in USDC units).
  - Repeat periodically, rate-limited.

Constraints:
- Merge can be executed any time after a condition has been prepared on the CTF contract.
- One unit of each position in a full set is burned in return for 1 unit of collateral.
- Merge is idempotent at the state level if confirmed by onchain receipt or relayer response.

Wallet mode / gas:
- If using Polymarket proxy wallet routed through relayer: Polymarket pays gas for CTF operations (split, merge, redeem).
- If using standalone EOA: we pay gas; therefore merge must be rate-limited and thresholded to avoid death-by-gas.

Config keys:
- merge_enabled (bool, default true)
- merge_min_sets (e.g., 25)
- merge_batch_sets (e.g., 25)
- merge_interval_s (e.g., 10)
- merge_max_ops_per_minute (e.g., 6)
- merge_pause_during_fast_move (bool, default true)
- merge_wallet_mode: RELAYER | EOA

Accounting:
- Merged collateral increases “available to quote” budget; this is a core performance lever.

We merge full sets during the market (capital velocity). Any remaining unmerged inventory is held to resolution.
Process:
1) Confirm market is closed (acceptingOrders false / resolved flag from Gamma or WS event)
2) Wait for resolution outcome (may be delayed)
3) Redeem winning tokens via CTF tooling / SDK (see Inventory Management doc guidance)
4) Record realized pnl:
   pnl = redeemed_usdc - total_costs - taker_fees + rebates + liquidity_rewards

Retries:
- exponential backoff
- alert if market unresolved after resolution_timeout_minutes

--------------------------------------------------------------------
11) Safety / Kill Switch
--------------------------------------------------------------------

Immediate cancel-all + stop trading triggers:
- Geoblock / restricted detected from API responses
- Auth failure repeated > N times
- STALE_ORACLE: Chainlink BTC/USD stale > rtds_chainlink_stale_ms
- Heartbeats failure > heartbeat_grace_ms
- CLOB market ws stale for both tokens > market_ws_stale_ms
- Too many order rejections in rolling window
- Inventory skew > max_unpaired_shares_global
- Daily drawdown > max_daily_loss_usdc (if configured)

When triggered:
- cancel all orders
- set bot state HALTED
- require manual restart (or config allow_auto_resume)

--------------------------------------------------------------------
12) Data Capture / Moat Flywheel (Required)
--------------------------------------------------------------------

We create a persistent event log containing:
- market slug, timestamps
- CLOB best_bid/ask + tick size updates
- RTDS BTC price updates (Chainlink BTC/USD + Binance BTCUSDT)
- desired quotes
- order submissions/cancels
- user fills and trade events
- periodic snapshots of inventory and pnl

Format:
- JSONL is acceptable; Parquet optional.

Why this is moat:
- Enables replay + parameter tuning + new alpha without guessing.
- Lets us measure adverse selection and fill quality, not just pnl.

--------------------------------------------------------------------
13) Acceptance Criteria (Alpha-first)
--------------------------------------------------------------------

A) Core correctness
- Tracks current and next markets, rolling every 15m.
- Maintains bids both sides with top-level combined cap and post-only constraints.
- Stops quoting and cancels orders 60s before end.
- Correctly tracks partial fills and inventory.

B) Alpha / Edge requirements
- Uses RTDS BTC feed with Chainlink as settlement truth; pauses quoting when Chainlink is stale (Binance stale => size down only).
- Uses probability model q_up and fair value caps; does not quote above caps in NORMAL mode.
- Dynamic target_total responds to volatility/time/spread.
- Implements multi-level ladder (>=2 levels default) with churn minimization.

C) Fees / Rebates
- Correctly fetches feeRateBps for signing and succeeds placing orders in fee-enabled markets.
- Implements taker fee formula for EV checks.
- Tracks maker fill "fee_equivalent" for rebate analytics.
- Supports checking order scoring and can adjust to be scoring (when enabled).

D) Ops
- Runs continuously in AWS eu-west-1.
- Exposes health endpoint + metrics.
- Graceful shutdown cancels orders.

### Performance & Stress Requirements (mandatory)
We must define and test performance targets:

Throughput targets (minimum):
- Market WS messages: sustain 2,000 msgs/sec without falling behind (no unbounded queue growth).
- RTDS updates: sustain 200 msgs/sec combined.
- User WS: sustain burst fill updates without missing order state.

Latency targets (p95 on a healthy host):
- From “book update received” -> “desired quotes computed”: <= 20ms
- From “desired quote changed” -> “REST submission attempted”: <= 100ms (excluding rate-limit waits)

Stress test harness:
- Provide a replay-driven stress test that replays recorded WS + RTDS logs at 1x and 5x speed.
- The bot must remain stable, bounded memory, and keep invariants (no cross, no post-cutoff orders).

--------------------------------------------------------------------
14) Config (must be explicit; no magic numbers)
--------------------------------------------------------------------

Required config keys (non-exhaustive):
- trading:
  - target_total_base
  - target_total_min
  - target_total_max
  - cutoff_seconds (default 60)
  - ladder_levels (>=2)
  - ladder_step_ticks
  - size_decay
  - base_size_shares_current
  - base_size_shares_next
  - min_order_size_shares
  - base_improve_ticks
  - max_improve_ticks
  - min_ticks_from_ask
  - reprice_min_ticks
  - resize_min_pct
  - min_update_interval_ms

- inventory:
  - skew_mild
  - skew_moderate
  - skew_severe
  - max_unpaired_shares_per_market
  - max_unpaired_shares_global
  - emergency_window_s
  - taker_window_s
  - completion_min_profit_per_share (deprecated; use completion.min_profit_per_share)
  - allow_taker_completion (deprecated; use completion.enabled)
  - allow_negative_completion (deprecated; use completion.max_loss_usdc + completion.order_type)

- alpha:
  - vol_halflife_s
  - drift_halflife_s
  - drift_clamp_per_s
  - var_ref
  - spread_ref
  - k_vol, k_spread, k_time, k_fast
  - fast_move_threshold_bps (deprecated; use oracle.fast_move_threshold_bps)
  - rtds_stale_ms (deprecated; use oracle.chainlink_stale_ms / oracle.binance_stale_ms)
  - rtds_divergence_threshold (deprecated; use oracle.oracle_disagree_threshold_bps)
  - divergence_hold_ms (deprecated)

- oracle:
  - chainlink_stale_ms
  - binance_stale_ms
  - fast_move_window_ms
  - fast_move_threshold_bps
  - oracle_disagree_threshold_bps

- merge:
  - enabled
  - min_sets
  - batch_sets
  - interval_s
  - max_ops_per_minute
  - pause_during_fast_move
  - wallet_mode (RELAYER | EOA)

- completion:
  - enabled
  - order_type (FAK | FOK)
  - max_loss_usdc
  - min_profit_per_share
  - use_explicit_price_cap (must be true)

- heartbeats:
  - enabled
  - interval_ms
  - grace_ms

- rewards:
  - enable_liquidity_rewards_chasing
  - require_scoring_orders (bool)
  - scoring_check_interval_s

- infra:
  - aws_region (eu-west-1)
  - log_level
  - metrics_port
  - health_port

- keys:
  - wallet_mode (EOA | PROXY_SAFE)
  - private_key_source (env | secrets_manager | kms)
  - api_creds_source (derive | explicit)
  - funder_address (if proxy mode)

--------------------------------------------------------------------
15) Moat / Edge Summary (explicit)
--------------------------------------------------------------------

We are competitive because:
1) Low-latency market making: Rust + WS-first + batching + minimal churn.
2) Oracle-aligned toxicity gating: Chainlink is settlement truth; Binance is fast-move warning only.
3) Fair-value caps + q_up model: prevents being systematically picked off by informed flow.
4) Incentive-aware quoting: integrates maker rebates math + liquidity scoring.
5) Capital velocity (merge loop): convert set edge into capital recycling during the market.
6) Data capture and replay: every run generates training/tuning data -> continuous improvement loop.
