# Strategy Soundness — Maker-Only Quoting on BTC 15m Up/Down

**Severity: HIGH**

This section evaluates whether the observed strategy logic (quotes, caps, inventory skew) is internally coherent for Polymarket BTC 15m markets, and whether it leaks taker behavior or exposes obvious pickoff risk.

## Scope note (what this does / doesn’t claim)

- This is not a “does it make money” claim. It’s “does it behave like a disciplined maker bot and avoid obvious self-inflicted losses.”
- Any Polymarket behavior not confirmed against official docs is marked **UNKNOWN** and paired with a concrete validation test.

## What the bot currently does (observed)

- **Maker-only quoting**: Every generated quote is `post_only: true`.
  - Evidence: `src/strategy/quote_engine.rs::build_ladder()` hardcodes `post_only: true`.
  - Order signing respects `post_only`: `src/clients/clob_rest.rs::build_signed_orders()` sets `.post_only(order.post_only)`.

- **Two-sided (Up + Down) buy quoting** with a combined top-level cap:
  - Computes per-side caps as `cap_up = q_up * target_total`, `cap_down = (1-q_up) * target_total` (`src/alpha/mod.rs`).
  - Enforces `p_up0 + p_down0 <= target_total` on level 0 (`src/strategy/quote_engine.rs::enforce_combined_cap()`).

- **Multi-level ladder**: Places `ladder_levels` levels per token, stepping down by `ladder_step_ticks * tick`.

- **Alpha gating**:
  - Quoting is disabled when Chainlink is stale or market WS is considered stale (via `size_scalar == 0` → `quoting_enabled` false).
  - Fast move reduces `target_total` and can reduce `size_scalar` (`src/alpha/mod.rs`, `src/alpha/target_total.rs`, `src/alpha/toxicity.rs`).

- **Inventory skew**: Increases size on the missing side and suppresses/removes the excess side based on unpaired shares (`src/strategy/risk.rs::adjust_for_inventory()`).

## Strengths (microstructure + correctness)

1) **No intentional taker behavior in quoting path**
   - Quote prices are constrained by `best_ask - min_ticks_from_ask * tick` and floored to tick.
   - This is aligned with “maker-only / post-only” requirements.

2) **Exposure-neutral structure via combined cap**
   - The combined cap (`p_up0 + p_down0 <= target_total`) is the correct primitive for set-completion acquisition.

3) **Strategy reacts on a sub-second cadence**
   - Quote ticks default to 200ms (`TradingConfig.quote_tick_interval_ms`).
   - For Polymarket, this is generally competitive vs UI-driven participants, assuming the rest of the pipeline does not stall (see latency section).

## CRITICAL strategy gaps / PnL leaks

### 1) End-of-interval inventory neutralization is missing (taker completion not wired)

- Evidence: `src/strategy/risk.rs::should_taker_complete()` is never called; no code submits completion orders.
- Impact:
  - You can enter the cutoff window with unpaired exposure and then stop quoting at `cutoff_ts_ms` (60s before end) without a deterministic neutralization mechanism.
  - This undermines “capital preservation > PnL”.
- Action:
  - Wire completion decision into execution.
  - Ensure completion uses **explicit price cap** and `FAK/FOK` only (already implemented in signing, but unused).

### 2) Strategy can quote in markets that are not tradable / restricted

- Evidence: `MarketIdentity` includes `active/closed/accepting_orders/restricted`, but quote generation ignores them. `quoting_enabled` ignores them too.
- Impact:
  - Orders may be rejected (wasted bandwidth, risk of rate limits).
  - More importantly: **restricted** can indicate compliance/geoblock conditions; spec requires cancel-all + halt.
- Action:
  - Gate quoting on these flags; if `restricted`, halt and cancel all.

### 3) Market WS staleness logic is mis-specified and can be unsafe in both directions

- Evidence: toxicity uses `alpha_cfg.rtds_stale_ms` (default 750ms) for `market_ws_stale` (`src/alpha/toxicity.rs`).
- Risk A (too tight): false halts/cancel churn in quiet markets.
- Risk B (too loose / wrong aggregation): current code uses the **latest** of Up/Down timestamps (max). If one token is stale while the other updates, the market may be treated as fresh.
- Action:
  - Add explicit `market_ws_stale_ms` config; compute per-token staleness and enforce per-token cancel/pause per spec.

## HIGH strategy risks (survivability in fast flips / thin liquidity)

1) **Tick size readiness is not enforced**
   - `MarketState` tick sizes default to 0.01 (`src/state/market_state.rs`).
   - Quote generation only checks tick size > 0, so it will quote with 0.01 until a `tick_size_change` event arrives.
   - In Polymarket, wrong tick size means:
     - non-competitive quotes, or
     - outright rejections (depending on matching rules).
   - Action: require tick size to be learned from WS/Gamma before quoting a token.

2) **Inventory skew thresholds appear extremely tight by default**
   - Defaults: `skew_severe = 0.50` shares (`src/config.rs`).
   - With base sizes of 3–5 shares, a single partial fill can force emergency behavior (drop entire excess side).
   - This may be intended for “capital preservation”, but it will materially reduce fill rate and may worsen adverse selection if you only quote the missing side in toxic conditions.
   - Action: validate thresholds vs typical fill sizes; consider expressing skew thresholds as multiples of `base_size` or as USDC notional.

3) **“Improve vs best_bid” can self-churn if best_bid is your own order**
   - Evidence: `top_price()` uses `best_bid + improve` as the competitiveness anchor (`src/strategy/quote_engine.rs`).
   - Problem: the market WS top-of-book does not include owner identity, so you can’t reliably know if `best_bid` is yours.
   - Failure mode: with `base_improve_ticks > 0`, you can end up cancel/reposting to “outbid yourself,” walking price upward until capped by `best_ask - min_ticks_from_ask*tick` (or until you hit your cap), bleeding queue position and rate limits.
   - Action:
     - Keep `base_improve_ticks = 0` unless you have a robust, external best-bid signal (e.g., full depth + self-order tagging via user feed).
     - If you enable “improve,” gate it on stable conditions (spread > X ticks, not fast-move, and not already at post-only cap).

## UNKNOWNs to validate (specific experiments)

1) **Market WS cadence under quiet books**
   - Experiment: subscribe to a BTC 15m token during a quiet interval; measure max gap between `best_bid_ask` / `price_change` events.
   - Use that to set `market_ws_stale_ms` to avoid churn while still catching disconnects.

2) **Tick size availability at subscription**
   - Experiment: on fresh subscribe, do you receive a tick size immediately? If not, ensure you fetch tick size from Gamma or infer safely.

3) **Maker-only compliance under forced-cross conditions (must-pass)**
   - Experiment: force the model/cap to imply `bid_price >= best_ask` and assert:
     - the bot clamps to `best_ask - tick` (or does not quote),
     - post-only rejects are handled cleanly (no infinite replace loop),
     - observed taker fills remain zero (except explicit completion).

4) **YES/NO coupling under trending BTC**
   - Experiment: replay a strongly trending interval and assert unpaired exposure remains within caps and decreases as skew logic activates.
