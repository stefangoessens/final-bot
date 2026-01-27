# Code Review — Strategy / Alpha / Edge

This file evaluates whether the **implemented strategy** matches the intended “edge = 1–2 cents + rebates” design, and what’s missing for real competitiveness.

---

## 1) What the bot is currently doing

### Quoting
- Multi-level ladder quoting on both outcomes.
- Enforces combined cost cap (target total) and top-of-book anchoring.
  - `src/strategy/quote_engine.rs` (`enforce_combined_cap`, `build_desired_orders`).

### Alpha / fair value
- Computes `q_up` with a GBM-style model:
  - start price from Chainlink,
  - current price from Chainlink/binance,
  - EWMA volatility,
  - `P(S_T > S_0)` for Up outcome.
  - `src/alpha/mod.rs`, `src/alpha/probability.rs`.

### Toxicity gating
- Compares chainlink vs binance; if divergent/stale, reduces size or disables quoting.
- Treats Chainlink as truth:
  - chainlink stale => size_scalar = 0 => quoting disabled.
  - `src/alpha/toxicity.rs`.

### Rewards integration
- Calls `/orders-scoring` and nudges L0 price/size if not scoring.
  - `src/strategy/reward_engine.rs`.

---

## 2) Does it align with the stated spec?

Yes, mostly:
- **Target combined buy cost ~0.98–0.99** is enforced via `target_total` and cap logic.
- **Chainlink is settlement truth** and chainlink stale disables quoting.
  - BTC 15m markets explicitly use Chainlink as the resolution source.
- **Completion uses bounded FOK/FAK** — but currently has a construction bug (critical fixes).

---

## 3) Is the alpha strong enough?

Probably not by itself.

A GBM model on 15-minute BTC is basically “coin flip with slightly wider tails”. It won’t reliably detect the toxic regimes where you get lifted on the wrong side.

Your real edge will mostly come from:
- maker rebates / rewards scoring,
- robust fast-move protection,
- efficient merge (capital velocity).

---

## 4) Concrete, implementable alpha improvements (minimal complexity)

### A) Blend model probability with market-implied probability
If the market is pricing Up at 0.55, and your model says 0.50, you shouldn’t necessarily slam 0.50 quotes—market can encode information you’re missing.

Add a blended fair value:

```rust
// in alpha/mod.rs (conceptual, fits your current structs)
fn blended_q_up(model_q: f64, up_mid: Option<f64>) -> f64 {
    let Some(mid) = up_mid else { return model_q; };
    let w_market = 0.35; // tune
    (1.0 - w_market) * model_q + w_market * mid
}
```

You can compute `up_mid` from top-of-book:

```rust
fn mid_from_book(best_bid: Option<f64>, best_ask: Option<f64>) -> Option<f64> {
    match (best_bid, best_ask) {
        (Some(b), Some(a)) if a > 0.0 && b > 0.0 && a >= b => Some(0.5 * (a + b)),
        _ => None,
    }
}
```

Then in `AlphaEngine::update_alpha`, after computing `q_up_model`:

```rust
let up_mid = mid_from_book(state.up.best_bid, state.up.best_ask);
let q_up = blended_q_up(q_up_model, up_mid).clamp(0.01, 0.99);
```

This is cheap, uses data you already have, and makes the bot less “model-pure” and more “market-aware”.

### B) Make quote width respond to volatility / toxicity
Right now you scale size by regime but don’t obviously widen the ladder on high vol.

Minimal change: add a `spread_ticks` multiplier in `QuoteEngine` based on `alpha.regime`:

```rust
let width_mult = match state.alpha.regime {
    AlphaRegime::Normal => 1.0,
    AlphaRegime::FastMove => 1.5,
    AlphaRegime::Toxic => 2.0,
    _ => 10.0, // effectively stop quoting already
};

let spread = cfg.spread_ticks as f64 * width_mult;
```

(You’d need to change `spread_ticks` type to f64 or do rounding. This keeps behavior simple.)

---

## 5) Completion (taker) strategy recommendation

Given 15m markets are high-throughput and adverse selection is real:
- keep maker-only as the default,
- allow taker completion **only when skew is large and EV is strongly positive**, and
- ensure completion orders are strictly bounded and sized correctly (fix bug first).

Your existing `RiskEngine::should_taker_complete` is reasonable as a starting point; validate with live logs.

---

## 6) What’s missing to make strategy “real”
1. **Backtesting / simulation**: even a simple replay of recorded WS frames into the quote/execution logic.
2. **Post-trade analytics**:
   - realized edge per fill,
   - PnL attribution by regime,
   - how often scoring nudges helped vs hurt.
3. **Parameter calibration** for:
   - oracle divergence thresholds,
   - size scalar multipliers,
   - drift/volatility half-life,
   - ladder spacing.

You already have good event logging to support this—just ensure it’s rotated and safe.
