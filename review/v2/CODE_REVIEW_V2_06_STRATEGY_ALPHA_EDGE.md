# CODE REVIEW (V2) — Strategy / Alpha / Edge

This file focuses on whether the **trading logic**, as implemented, matches your goals and whether it plausibly produces edge.

---

## What the strategy is actually doing today

### Quoting model
- You compute `q_up` as a GBM probability of finishing above the strike:
  - `s0`: first Chainlink price observed at/after interval start
  - `st`: latest Chainlink (preferred) or Binance fallback
  - `tau`: time-to-expiry
  - `drift`: effectively **disabled by default** (`drift_clamp_per_s = 0`)
  - `var`: EWMA realized variance

- You compute `target_total` (combined cap) using:
  - base `target_total_base` (e.g., 0.985)
  - adjustments vs volatility/spread/time/regime

- You set:
  - `cap_up = q_up * target_total`
  - `cap_down = (1-q_up) * target_total`

- You place **post-only bids** at multiple levels below each cap (ladder).

### Inventory logic
- If you accumulate unpaired exposure, inventory skew reduces sizes on the “rich” side.
- Near the end:
  - you stop quoting at cutoff (T-60s by default)
  - you can run taker completion inside the final window (T-30s by default)

### Liquidity rewards
- You added a minimal scoring nudge:
  - checks if the level-0 order IDs are scoring
  - nudges price/size within small limits if not

This is a coherent baseline MM strategy for “full set arb” + rewards.

---

## Reality check: where edge could come from
On these 15m BTC markets, you only have a few plausible edges:
1) **Spread capture / microstructure:** buy full sets below 1.00 reliably.
2) **Maker rebates / scoring:** earn enough rebates to overcome adverse selection + taker fees.
3) **Toxicity gating:** avoid being picked off on fast moves.

Your code now has (3) and a first version of (2). (1) depends on actual market conditions.

---

## Concrete improvements that are “small” but high leverage (with code)

### P1 — Blend your model probability with market-implied probability
Your GBM model can be wrong in two common ways:
- jump risk / fat tails (BTC moves are not Gaussian)
- the market itself already prices the move (information in the book)

A simple, low-risk improvement is to **blend**:
- `q_model` (your GBM probability)
- `q_mkt` (implied from top-of-book mid prices)

#### Patch: add `q_market_blend` to `AlphaConfig`
File: `src/config.rs`

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct AlphaConfig {
    // ...
    #[serde(default = "default_q_market_blend")]
    pub q_market_blend: f64, // 0.0 = current behavior
}

fn default_q_market_blend() -> f64 { 0.0 }
```

#### Patch: compute `q_mkt` and blend in `alpha/mod.rs`
File: `src/alpha/mod.rs`

Add helper:

```rust
fn implied_q_from_books(up: &TokenBookTop, down: &TokenBookTop) -> Option<f64> {
    let up_mid = match (up.best_bid, up.best_ask) {
        (Some(b), Some(a)) if a >= b => Some((a + b) * 0.5),
        _ => None,
    }?;
    let down_mid = match (down.best_bid, down.best_ask) {
        (Some(b), Some(a)) if a >= b => Some((a + b) * 0.5),
        _ => None,
    }?;
    let denom = up_mid + down_mid;
    if denom <= 0.0 { return None; }
    Some((up_mid / denom).clamp(0.0, 1.0))
}
```

Then, inside `update_alpha`:

```rust
let q_model = compute_q_up(market_state, now_ms, drift_per_s, var_per_s);

let q_mkt = implied_q_from_books(&market_state.up_book, &market_state.down_book)
    .unwrap_or(q_model);

let w = alpha_cfg.q_market_blend.clamp(0.0, 1.0);
let q_up = (w * q_mkt + (1.0 - w) * q_model).clamp(0.0, 1.0);
```

This does **not** create a new “alpha model” — it reduces model error by anchoring to the market’s current belief. You can turn it on gradually by setting `q_market_blend` to something like `0.2`.

---

### P2 — Add a fill-toxicity feedback loop (very small)
You already have fast-move gating, but you can also detect *your own adverse selection*:
- if your fills cluster right before unfavorable price moves, widen quotes / reduce size.

Minimal (no heavy infra):
- track a rolling window of `(fill_ts, fill_side, mid_before, mid_after_500ms)`
- if `avg(markout)` is negative beyond threshold, reduce size_scalar for N seconds

#### Code sketch: store markout state in `MarketState.alpha`
File: `src/state/market_state.rs` (add fields)

```rust
pub struct AlphaState {
    // ...
    pub last_fill_ts_ms: i64,
    pub last_fill_mid: f64,
    pub markout_ewma: f64,
}
```

Then in `StateManager` on fill events:
- record current mid
- schedule a delayed check (or update when next RTDS tick arrives)

This is a bigger patch so I’m only providing the skeleton, but it is the next “high signal” feature if you want real edge beyond static caps.

---

## Final assessment on edge
- Your strategy is **internally consistent** and now supports rewards + merges.
- The remaining question is whether the combination of:
  - `target_total` (0.98–0.99),
  - maker rebates,
  - and toxicity gating
is sufficient to overcome adverse selection and fees.

The **fastest way to answer** is not more theory — it’s:
1) enable full event logging (already implemented)
2) paper trade for 24–72 hours
3) compute realized:
   - average full-set cost vs 1.00
   - rebates per $ volume
   - fill markouts
   - completion costs

Your codebase is now in a state where those measurements are feasible.

