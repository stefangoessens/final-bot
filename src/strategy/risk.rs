#![allow(dead_code)]

use crate::config::{CompletionConfig, CompletionOrderType, InventoryConfig, TradingConfig};
use crate::state::market_state::MarketState;
use crate::strategy::fee;
use crate::strategy::DesiredOrder;
use polymarket_client_sdk::clob::types::Side;

const UNPAIRED_EPS: f64 = 1e-6;
const MAX_LOSS_PER_SHARE_CAP: f64 = 0.99;
const COMPLETION_LIMIT_PRICE_ABS_MAX: f64 = 0.99;

#[derive(Debug, Clone)]
pub struct TakerAction {
    pub token_id: String,
    pub side: Side,
    pub p_max: f64,
    pub shares: f64,
    pub order_type: CompletionOrderType,
}

pub fn adjust_for_inventory(
    desired: &mut Vec<DesiredOrder>,
    state: &MarketState,
    cfg: &InventoryConfig,
    _now_ms: i64,
) {
    if desired.is_empty() {
        return;
    }

    let up_shares = state.inventory.up.shares;
    let down_shares = state.inventory.down.shares;
    let unpaired = (up_shares - down_shares).abs();
    if unpaired <= UNPAIRED_EPS {
        let has_up = desired
            .iter()
            .any(|order| order.token_id == state.identity.token_up);
        let has_down = desired
            .iter()
            .any(|order| order.token_id == state.identity.token_down);
        if has_up ^ has_down {
            desired.clear();
        }
        return;
    }

    let (missing_token, excess_token) = if up_shares < down_shares {
        (
            state.identity.token_up.as_str(),
            state.identity.token_down.as_str(),
        )
    } else {
        (
            state.identity.token_down.as_str(),
            state.identity.token_up.as_str(),
        )
    };

    let missing_mult = if unpaired > cfg.skew_mild {
        let denom = cfg.skew_moderate - cfg.skew_mild;
        if denom > 0.0 {
            let ratio = ((unpaired - cfg.skew_mild) / denom).clamp(0.0, 1.0);
            1.0 + ratio
        } else {
            1.0
        }
    } else {
        1.0
    };

    for order in desired.iter_mut() {
        if order.token_id == missing_token {
            order.size *= missing_mult;
        }
    }

    // Strict pairing: when unpaired, do not quote *any* levels on the excess side.
    desired.retain(|order| order.token_id != excess_token);
}

pub fn should_taker_complete(
    state: &MarketState,
    inventory_cfg: &InventoryConfig,
    completion_cfg: &CompletionConfig,
    trading_cfg: &TradingConfig,
    now_ms: i64,
) -> Option<TakerAction> {
    if !completion_cfg.enabled || !completion_cfg.use_explicit_price_cap {
        return None;
    }

    let up_shares = state.inventory.up.shares;
    let down_shares = state.inventory.down.shares;
    let unpaired = (up_shares - down_shares).abs();
    if unpaired <= UNPAIRED_EPS {
        return None;
    }

    let time_to_cancel_ms = state.cutoff_ts_ms - now_ms;
    let time_to_cancel_s = time_to_cancel_ms / 1000;
    let hard_cap = inventory_cfg
        .max_unpaired_shares_per_market
        .min(inventory_cfg.max_unpaired_shares_global);
    let hardcap_breached = unpaired >= hard_cap.max(0.0) - UNPAIRED_EPS;
    let severe = unpaired > inventory_cfg.skew_severe;
    let emergency =
        severe || hardcap_breached || time_to_cancel_s <= inventory_cfg.emergency_window_s;

    // FULL_SPEC 7.6(D),7.8: taker completion is time-gated; do not taker-complete purely due to skew.
    if time_to_cancel_s > inventory_cfg.taker_window_s && !hardcap_breached {
        return None;
    }

    let (missing_token, missing_ask, excess_avg_cost, tick_size) = if up_shares < down_shares {
        (
            state.identity.token_up.as_str(),
            state.up_book.best_ask,
            state.inventory.down.avg_cost(),
            state.up_book.tick_size,
        )
    } else {
        (
            state.identity.token_down.as_str(),
            state.down_book.best_ask,
            state.inventory.up.avg_cost(),
            state.down_book.tick_size,
        )
    };

    let avg_cost = excess_avg_cost?;
    let min_profit = if emergency && completion_cfg.max_loss_usdc > 0.0 {
        let max_loss_per_share = (completion_cfg.max_loss_usdc / unpaired).max(0.0);
        -max_loss_per_share.min(MAX_LOSS_PER_SHARE_CAP)
    } else {
        completion_cfg.min_profit_per_share
    };

    let min_quote_price = trading_cfg.min_quote_price.max(0.0);
    let best_ask = missing_ask?;

    // When the missing side is below our quote floor, avoid buying it as a completion action.
    // Instead, prefer selling the excess side to reduce unpaired exposure.
    if best_ask < min_quote_price {
        let (excess_token, excess_bid, excess_tick) = if up_shares < down_shares {
            (
                state.identity.token_down.as_str(),
                state.down_book.best_bid,
                state.down_book.tick_size,
            )
        } else {
            (
                state.identity.token_up.as_str(),
                state.up_book.best_bid,
                state.up_book.tick_size,
            )
        };

        let best_bid = excess_bid?;
        if best_bid < min_quote_price {
            return None;
        }
        let cap = completion_limit_price_ceiling(excess_tick);
        let p_min_raw = min_taker_sell_price(avg_cost, min_profit, unpaired)?.max(min_quote_price);
        if p_min_raw > cap + 1e-12 {
            return None;
        }
        let p_min = round_up_to_tick(p_min_raw, excess_tick)?;
        if p_min > cap + 1e-12 {
            return None;
        }
        if best_bid + 1e-12 < p_min {
            return None;
        }

        return Some(TakerAction {
            token_id: excess_token.to_string(),
            side: Side::Sell,
            p_max: p_min,
            shares: unpaired,
            order_type: completion_cfg.order_type,
        });
    }

    let cap = completion_limit_price_ceiling(tick_size);
    let p_max_raw = max_taker_price(avg_cost, min_profit, unpaired)?.min(cap);
    let p_max = round_down_to_tick(p_max_raw, tick_size)?;
    if p_max <= 0.0 {
        return None;
    }
    if p_max < min_quote_price {
        return None;
    }
    if best_ask > p_max + 1e-12 {
        return None;
    }

    Some(TakerAction {
        token_id: missing_token.to_string(),
        side: Side::Buy,
        p_max,
        shares: unpaired,
        order_type: completion_cfg.order_type,
    })
}

pub fn apply_skew_repair_pricing(
    desired: &mut Vec<DesiredOrder>,
    state: &MarketState,
    trading_cfg: &TradingConfig,
    inventory_cfg: &InventoryConfig,
    completion_cfg: &CompletionConfig,
    now_ms: i64,
) {
    if desired.is_empty() {
        return;
    }

    let up_shares = state.inventory.up.shares;
    let down_shares = state.inventory.down.shares;
    let unpaired = (up_shares - down_shares).abs();
    if unpaired <= UNPAIRED_EPS {
        return;
    }

    let (missing_token, missing_best_ask, tick_size, excess_avg_cost) = if up_shares < down_shares {
        (
            state.identity.token_up.as_str(),
            state.up_book.best_ask,
            state.up_book.tick_size,
            state.inventory.down.avg_cost(),
        )
    } else {
        (
            state.identity.token_down.as_str(),
            state.down_book.best_ask,
            state.down_book.tick_size,
            state.inventory.up.avg_cost(),
        )
    };

    if !desired.iter().any(|o| o.token_id == missing_token) {
        return;
    }

    let Some(avg_cost) = excess_avg_cost else {
        return;
    };

    let time_to_cancel_ms = state.cutoff_ts_ms - now_ms;
    let time_to_cancel_s = time_to_cancel_ms / 1000;

    let hard_cap = inventory_cfg
        .max_unpaired_shares_per_market
        .min(inventory_cfg.max_unpaired_shares_global);
    let hardcap_breached = unpaired >= hard_cap.max(0.0) - UNPAIRED_EPS;
    let severe = unpaired > inventory_cfg.skew_severe;
    let emergency =
        severe || hardcap_breached || time_to_cancel_s <= inventory_cfg.emergency_window_s;

    // Hard cap derived from set-completion math (conservative: uses taker fee curve).
    let min_profit = if emergency && completion_cfg.max_loss_usdc > 0.0 {
        let max_loss_per_share = (completion_cfg.max_loss_usdc / unpaired).max(0.0);
        -max_loss_per_share.min(MAX_LOSS_PER_SHARE_CAP)
    } else {
        completion_cfg.min_profit_per_share
    };
    let cap_ceiling = completion_limit_price_ceiling(tick_size);
    let Some(cap_raw) = max_taker_price(avg_cost, min_profit, unpaired).map(|v| v.min(cap_ceiling))
    else {
        desired.retain(|o| o.token_id != missing_token);
        return;
    };
    let Some(cap) = round_down_to_tick(cap_raw, tick_size) else {
        return;
    };
    if cap <= 0.0 {
        desired.retain(|o| o.token_id != missing_token);
        return;
    }

    // Target: bid as close to best ask as allowed (post-only), but never above `cap`.
    let mut max_post_only = cap;
    if let Some(best_ask) = missing_best_ask {
        if tick_size.is_finite() && tick_size > 0.0 {
            let po = best_ask - (trading_cfg.min_ticks_from_ask as f64) * tick_size;
            max_post_only = max_post_only.min(po);
        }
    }
    let Some(max_post_only) = round_down_to_tick(max_post_only, tick_size) else {
        return;
    };

    if max_post_only < trading_cfg.min_quote_price - 1e-12 {
        desired.retain(|o| o.token_id != missing_token);
        return;
    }

    let step = if tick_size.is_finite() && tick_size > 0.0 {
        (trading_cfg.ladder_step_ticks as f64) * tick_size
    } else {
        0.0
    };

    for order in desired.iter_mut().filter(|o| o.token_id == missing_token) {
        order.post_only = true;

        let level_offset = (order.level as f64) * step;
        let raw = (max_post_only - level_offset).max(0.0);
        let Some(target) = round_down_to_tick(raw, tick_size) else {
            continue;
        };

        if target < trading_cfg.min_quote_price - 1e-12 {
            order.price = 0.0;
            continue;
        }

        let mut new_price = order.price;
        if !new_price.is_finite() || new_price < 0.0 {
            new_price = target;
        }
        if new_price < target {
            new_price = target;
        }
        if new_price > max_post_only {
            new_price = max_post_only;
        }
        if new_price > cap {
            new_price = cap;
        }
        order.price = new_price;
    }

    // Never output prices below the quote floor.
    desired.retain(|o| o.price >= trading_cfg.min_quote_price - 1e-12);
}

fn completion_limit_price_ceiling(tick: f64) -> f64 {
    // Use a tick-aligned ceiling so that round_up_to_tick never rounds a completion limit up to 1.0
    // on coarse-tick markets (e.g. 0.99 with tick=0.05 would otherwise round to 1.0).
    round_down_to_tick(COMPLETION_LIMIT_PRICE_ABS_MAX, tick)
        .unwrap_or(COMPLETION_LIMIT_PRICE_ABS_MAX)
}

fn max_taker_price(avg_cost: f64, min_profit: f64, shares: f64) -> Option<f64> {
    if !avg_cost.is_finite() || !min_profit.is_finite() || !shares.is_finite() || shares <= 0.0 {
        return None;
    }

    let target = 1.0 - avg_cost - min_profit;
    if target <= 0.0 {
        return None;
    }
    if target >= 1.0 {
        return Some(1.0);
    }

    let mut lo = 0.0;
    let mut hi = 1.0;
    for _ in 0..48 {
        let mid = (lo + hi) * 0.5;
        let fee_per_share = fee::taker_fee_usdc(shares, mid) / shares;
        let spend = mid + fee_per_share;
        if spend > target {
            hi = mid;
        } else {
            lo = mid;
        }
    }

    Some(lo)
}

fn min_taker_sell_price(avg_cost: f64, min_profit: f64, shares: f64) -> Option<f64> {
    if !avg_cost.is_finite() || !min_profit.is_finite() || !shares.is_finite() || shares <= 0.0 {
        return None;
    }

    let target = avg_cost + min_profit;
    if target <= 0.0 {
        return Some(0.0);
    }

    let receive_at_one = 1.0 - fee::taker_fee_usdc(shares, 1.0) / shares;
    if receive_at_one + 1e-12 < target {
        return None;
    }

    let mut lo = 0.0;
    let mut hi = 1.0;
    for _ in 0..48 {
        let mid = (lo + hi) * 0.5;
        let fee_per_share = fee::taker_fee_usdc(shares, mid) / shares;
        let receive = mid - fee_per_share;
        if receive >= target {
            hi = mid;
        } else {
            lo = mid;
        }
    }

    Some(hi)
}

fn round_down_to_tick(price: f64, tick: f64) -> Option<f64> {
    if !price.is_finite() || price < 0.0 {
        return None;
    }
    if !tick.is_finite() || tick <= 0.0 {
        return Some(price);
    }
    // Guard against floating-point error at tick boundaries (e.g. 0.22 - 0.01 = 0.209999...).
    // We allow a tiny epsilon relative to tick size to avoid flooring one tick too low.
    let eps = (tick.abs() * 1e-6).max(1e-12);
    let steps = ((price + eps) / tick).floor();
    if !steps.is_finite() {
        return None;
    }
    let mut rounded = steps * tick;
    if rounded > price + eps {
        rounded = (steps - 1.0) * tick;
    }
    if !rounded.is_finite() || rounded < 0.0 {
        return None;
    }
    Some(rounded)
}

fn round_up_to_tick(price: f64, tick: f64) -> Option<f64> {
    if !price.is_finite() || price < 0.0 {
        return None;
    }
    if !tick.is_finite() || tick <= 0.0 {
        return Some(price);
    }
    let eps = (tick.abs() * 1e-6).max(1e-12);
    let steps = ((price - eps) / tick).ceil();
    if !steps.is_finite() {
        return None;
    }
    let mut rounded = steps * tick;
    if rounded + eps < price {
        rounded = (steps + 1.0) * tick;
    }
    if rounded > 1.0 + eps {
        return None;
    }
    if rounded > 1.0 && rounded <= 1.0 + eps {
        rounded = 1.0;
    }
    if !rounded.is_finite() || rounded < 0.0 {
        return None;
    }
    Some(rounded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CompletionConfig;
    use crate::state::market_state::{MarketIdentity, MarketState};
    use crate::strategy::TimeInForce;
    use polymarket_client_sdk::clob::types::Side;

    fn make_state(cutoff_ts_ms: i64) -> MarketState {
        let identity = MarketIdentity {
            slug: "btc-updown-15m-0".to_string(),
            interval_start_ts: 0,
            interval_end_ts: 0,
            condition_id: "cond".to_string(),
            token_up: "UP".to_string(),
            token_down: "DOWN".to_string(),
            active: true,
            closed: false,
            accepting_orders: true,
            restricted: false,
        };
        MarketState::new(identity, cutoff_ts_ms)
    }

    #[test]
    fn unpaired_strictly_removes_excess_side_orders() {
        let mut state = make_state(10_000_000);
        state.inventory.up.shares = 0.3;
        state.inventory.up.notional_usdc = 0.3;
        state.inventory.down.shares = 0.0;

        let mut desired = vec![
            DesiredOrder {
                token_id: state.identity.token_up.clone(),
                level: 0,
                price: 0.4,
                size: 1.0,
                post_only: true,
                tif: TimeInForce::Gtc,
            },
            DesiredOrder {
                token_id: state.identity.token_up.clone(),
                level: 1,
                price: 0.39,
                size: 1.0,
                post_only: true,
                tif: TimeInForce::Gtc,
            },
            DesiredOrder {
                token_id: state.identity.token_down.clone(),
                level: 0,
                price: 0.4,
                size: 1.0,
                post_only: true,
                tif: TimeInForce::Gtc,
            },
        ];

        let cfg = InventoryConfig::default();
        adjust_for_inventory(&mut desired, &state, &cfg, 0);

        assert!(
            !desired.iter().any(|o| o.token_id == "UP"),
            "expected all excess-side orders to be removed"
        );
        assert!(
            desired.iter().any(|o| o.token_id == "DOWN" && o.level == 0),
            "expected missing side level0 to remain"
        );
    }

    #[test]
    fn balanced_inventory_clears_one_sided_desired() {
        let state = make_state(10_000_000);
        let mut desired = vec![DesiredOrder {
            token_id: state.identity.token_up.clone(),
            level: 0,
            price: 0.4,
            size: 1.0,
            post_only: true,
            tif: TimeInForce::Gtc,
        }];

        let cfg = InventoryConfig::default();
        adjust_for_inventory(&mut desired, &state, &cfg, 0);
        assert!(
            desired.is_empty(),
            "expected one-sided desired to be cleared"
        );
    }

    #[test]
    fn taker_completion_requires_profit_threshold() {
        let mut state = make_state(30_000);
        state.inventory.up.shares = 1.0;
        state.inventory.up.notional_usdc = 0.6; // avg cost 0.6
        state.inventory.down.shares = 0.0;
        state.down_book.best_ask = Some(0.5);

        let mut inventory_cfg = InventoryConfig::default();
        inventory_cfg.taker_window_s = 30;

        let mut completion_cfg = CompletionConfig::default();
        completion_cfg.enabled = true;
        completion_cfg.order_type = CompletionOrderType::Fok;
        completion_cfg.min_profit_per_share = 0.01;
        completion_cfg.max_loss_usdc = 0.0;

        let trading_cfg = TradingConfig::default();
        let none_action =
            should_taker_complete(&state, &inventory_cfg, &completion_cfg, &trading_cfg, 0);
        assert!(none_action.is_none(), "expected no taker completion");

        state.inventory.up.notional_usdc = 0.2; // avg cost 0.2
        state.down_book.best_ask = Some(0.25);

        let some_action =
            should_taker_complete(&state, &inventory_cfg, &completion_cfg, &trading_cfg, 0)
                .expect("expected taker completion");
        assert!(
            some_action.p_max >= state.down_book.best_ask.unwrap(),
            "expected p_max >= best ask"
        );
        assert_eq!(some_action.side, Side::Buy);
        assert_eq!(some_action.order_type, CompletionOrderType::Fok);
    }

    #[test]
    fn taker_completion_blocks_if_ask_above_p_max() {
        let mut state = make_state(30_000);
        state.inventory.up.shares = 1.0;
        state.inventory.up.notional_usdc = 0.6; // avg cost 0.6
        state.inventory.down.shares = 0.0;

        let mut inventory_cfg = InventoryConfig::default();
        inventory_cfg.taker_window_s = 30;
        let mut completion_cfg = CompletionConfig::default();
        completion_cfg.enabled = true;
        completion_cfg.order_type = CompletionOrderType::Fok;
        completion_cfg.min_profit_per_share = 0.01;
        completion_cfg.max_loss_usdc = 0.0;

        let p_max = max_taker_price(0.6, 0.01, 1.0).expect("p_max should be computable");
        state.down_book.best_ask = Some(p_max + 0.05);

        let trading_cfg = TradingConfig::default();
        let action =
            should_taker_complete(&state, &inventory_cfg, &completion_cfg, &trading_cfg, 0);
        assert!(action.is_none(), "expected completion to be blocked");
    }

    #[test]
    fn taker_completion_can_trigger_early_when_hardcap_breached() {
        let mut state = make_state(300_000);
        state.inventory.up.shares = 1.0;
        state.inventory.up.notional_usdc = 0.2; // avg cost 0.2
        state.inventory.down.shares = 0.0;
        state.down_book.best_ask = Some(0.25);

        let mut inventory_cfg = InventoryConfig::default();
        inventory_cfg.taker_window_s = 30;
        inventory_cfg.max_unpaired_shares_per_market = 1.0;
        inventory_cfg.max_unpaired_shares_global = 1.0;

        let mut completion_cfg = CompletionConfig::default();
        completion_cfg.enabled = true;
        completion_cfg.order_type = CompletionOrderType::Fok;
        completion_cfg.min_profit_per_share = 0.0;
        completion_cfg.max_loss_usdc = 0.0;

        let trading_cfg = TradingConfig::default();
        let action =
            should_taker_complete(&state, &inventory_cfg, &completion_cfg, &trading_cfg, 0)
                .expect("expected early taker completion when hardcap breached");
        assert_eq!(action.token_id, "DOWN");
        assert_eq!(action.side, Side::Buy);
        assert!(action.p_max >= state.down_book.best_ask.unwrap() - 1e-12);
    }

    #[test]
    fn taker_completion_controlled_loss_allows_higher_ask() {
        let mut state = make_state(30_000);
        state.inventory.up.shares = 1.0;
        state.inventory.up.notional_usdc = 0.8; // avg cost 0.8
        state.inventory.down.shares = 0.0;
        state.down_book.best_ask = Some(0.22);

        let mut inventory_cfg = InventoryConfig::default();
        inventory_cfg.taker_window_s = 30;

        let mut completion_cfg = CompletionConfig::default();
        completion_cfg.enabled = true;
        completion_cfg.order_type = CompletionOrderType::Fok;
        completion_cfg.min_profit_per_share = 0.01;
        completion_cfg.max_loss_usdc = 0.0;

        let trading_cfg = TradingConfig::default();
        let no_loss =
            should_taker_complete(&state, &inventory_cfg, &completion_cfg, &trading_cfg, 0);
        assert!(
            no_loss.is_none(),
            "expected completion to be blocked without controlled-loss"
        );

        completion_cfg.max_loss_usdc = 0.1;
        let with_loss =
            should_taker_complete(&state, &inventory_cfg, &completion_cfg, &trading_cfg, 0)
                .expect("expected completion with controlled-loss");
        assert_eq!(with_loss.side, Side::Buy);
        assert!(
            with_loss.p_max >= 0.22 - 1e-12,
            "expected controlled-loss to raise p_max enough to cross the ask"
        );
    }

    #[test]
    fn taker_completion_sells_excess_when_missing_ask_below_quote_floor() {
        let mut state = make_state(30_000);
        state.inventory.up.shares = 1.0;
        state.inventory.up.notional_usdc = 0.85; // avg cost 0.85 -> p_max ~0.15
        state.inventory.down.shares = 0.0;
        state.down_book.best_ask = Some(0.10);
        state.down_book.tick_size = 0.01;
        state.up_book.best_bid = Some(0.90);
        state.up_book.tick_size = 0.01;

        let mut inventory_cfg = InventoryConfig::default();
        inventory_cfg.taker_window_s = 30;

        let mut completion_cfg = CompletionConfig::default();
        completion_cfg.enabled = true;
        completion_cfg.order_type = CompletionOrderType::Fok;
        completion_cfg.min_profit_per_share = 0.0;
        completion_cfg.max_loss_usdc = 0.0;

        let trading_cfg = TradingConfig::default();
        let action =
            should_taker_complete(&state, &inventory_cfg, &completion_cfg, &trading_cfg, 0)
                .expect("expected sell completion");
        assert_eq!(action.token_id, "UP");
        assert_eq!(action.side, Side::Sell);
        assert!(
            action.p_max <= state.up_book.best_bid.unwrap() + 1e-12,
            "expected sell limit <= best bid for marketability"
        );
    }

    #[test]
    fn taker_completion_sell_clamps_to_quote_floor() {
        let mut state = make_state(30_000);
        state.inventory.up.shares = 1.0;
        state.inventory.up.notional_usdc = 0.10; // avg cost 0.10 => p_min below floor
        state.inventory.down.shares = 0.0;
        state.down_book.best_ask = Some(0.15); // below floor triggers sell
        state.down_book.tick_size = 0.01;
        state.up_book.best_bid = Some(0.90);
        state.up_book.tick_size = 0.01;

        let mut inventory_cfg = InventoryConfig::default();
        inventory_cfg.taker_window_s = 30;

        let mut completion_cfg = CompletionConfig::default();
        completion_cfg.enabled = true;
        completion_cfg.order_type = CompletionOrderType::Fok;
        completion_cfg.min_profit_per_share = 0.0;
        completion_cfg.max_loss_usdc = 0.0;

        let trading_cfg = TradingConfig::default(); // min_quote_price = 0.20
        let action =
            should_taker_complete(&state, &inventory_cfg, &completion_cfg, &trading_cfg, 0)
                .expect("expected sell completion");
        assert_eq!(action.side, Side::Sell);
        assert!(
            action.p_max + 1e-12 >= trading_cfg.min_quote_price,
            "expected sell limit to be clamped to quote floor"
        );
    }

    #[test]
    fn taker_completion_sell_requires_excess_best_bid() {
        let mut state = make_state(300_000);
        state.inventory.up.shares = 1.0;
        state.inventory.up.notional_usdc = 0.2; // avg cost 0.2 -> p_max well above the floor
        state.inventory.down.shares = 0.0;
        state.down_book.best_ask = Some(0.15);
        state.down_book.tick_size = 0.01;

        let mut inventory_cfg = InventoryConfig::default();
        inventory_cfg.taker_window_s = 30;

        let mut completion_cfg = CompletionConfig::default();
        completion_cfg.enabled = true;
        completion_cfg.order_type = CompletionOrderType::Fok;
        completion_cfg.min_profit_per_share = 0.0;
        completion_cfg.max_loss_usdc = 0.0;

        let trading_cfg = TradingConfig::default();
        let action =
            should_taker_complete(&state, &inventory_cfg, &completion_cfg, &trading_cfg, 0);
        assert!(
            action.is_none(),
            "expected sell completion to be blocked when excess best bid is missing"
        );
    }

    #[test]
    fn taker_completion_none_when_missing_best_ask_is_missing() {
        let mut state = make_state(300_000);
        // Unpaired: excess=UP, missing=DOWN.
        state.inventory.up.shares = 1.0;
        state.inventory.up.notional_usdc = 0.2;
        state.inventory.down.shares = 0.0;
        state.down_book.best_ask = None;

        let mut inventory_cfg = InventoryConfig::default();
        inventory_cfg.taker_window_s = 30;

        let mut completion_cfg = CompletionConfig::default();
        completion_cfg.enabled = true;
        completion_cfg.order_type = CompletionOrderType::Fok;
        completion_cfg.min_profit_per_share = 0.0;
        completion_cfg.max_loss_usdc = 0.0;

        let trading_cfg = TradingConfig::default();
        let action =
            should_taker_complete(&state, &inventory_cfg, &completion_cfg, &trading_cfg, 0);
        assert!(
            action.is_none(),
            "expected no completion when missing best ask is unavailable"
        );
    }

    #[test]
    fn taker_completion_sell_requires_excess_best_bid_at_or_above_floor() {
        let mut state = make_state(300_000);
        // Unpaired: excess=UP, missing=DOWN.
        state.inventory.up.shares = 1.0;
        state.inventory.up.notional_usdc = 0.2;
        state.inventory.down.shares = 0.0;

        // Missing side under floor triggers sell path, but excess best bid is also under floor.
        state.down_book.best_ask = Some(0.15);
        state.down_book.tick_size = 0.01;
        state.up_book.best_bid = Some(0.19);
        state.up_book.tick_size = 0.01;

        let mut inventory_cfg = InventoryConfig::default();
        inventory_cfg.taker_window_s = 30;

        let mut completion_cfg = CompletionConfig::default();
        completion_cfg.enabled = true;
        completion_cfg.order_type = CompletionOrderType::Fok;
        completion_cfg.min_profit_per_share = 0.0;
        completion_cfg.max_loss_usdc = 0.0;

        let trading_cfg = TradingConfig::default(); // floor=0.20
        let action =
            should_taker_complete(&state, &inventory_cfg, &completion_cfg, &trading_cfg, 0);
        assert!(
            action.is_none(),
            "expected sell completion to be blocked when excess best bid is below quote floor"
        );
    }

    #[test]
    fn skew_repair_pricing_prunes_below_floor_levels() {
        let mut state = make_state(300_000);
        state.inventory.up.shares = 1.0;
        state.inventory.up.notional_usdc = 0.7; // avg cost 0.7 => completion cap above 0.20
        state.inventory.down.shares = 0.0;
        state.down_book.best_ask = Some(0.22);
        state.down_book.tick_size = 0.01;

        let trading_cfg = TradingConfig::default();
        let inventory_cfg = InventoryConfig::default();
        let mut completion_cfg = CompletionConfig::default();
        completion_cfg.min_profit_per_share = 0.0;
        completion_cfg.max_loss_usdc = 0.0;

        let mut desired = vec![
            DesiredOrder {
                token_id: state.identity.token_down.clone(),
                level: 0,
                price: 0.1,
                size: 1.0,
                post_only: false,
                tif: TimeInForce::Gtc,
            },
            DesiredOrder {
                token_id: state.identity.token_down.clone(),
                level: 1,
                price: 0.1,
                size: 1.0,
                post_only: false,
                tif: TimeInForce::Gtc,
            },
            DesiredOrder {
                token_id: state.identity.token_down.clone(),
                level: 2,
                price: 0.1,
                size: 1.0,
                post_only: false,
                tif: TimeInForce::Gtc,
            },
        ];

        apply_skew_repair_pricing(
            &mut desired,
            &state,
            &trading_cfg,
            &inventory_cfg,
            &completion_cfg,
            0,
        );

        assert!(
            desired
                .iter()
                .all(|o| o.price + 1e-12 >= trading_cfg.min_quote_price),
            "expected all remaining prices to respect the quote floor"
        );
        assert!(
            !desired.iter().any(|o| o.level == 2),
            "expected deep levels below the floor to be pruned"
        );

        let l0 = desired
            .iter()
            .find(|o| o.level == 0)
            .expect("level 0 should remain");
        let l1 = desired
            .iter()
            .find(|o| o.level == 1)
            .expect("level 1 should remain");
        assert!((l0.price - 0.21).abs() < 1e-6, "expected level0 ~0.21");
        assert!((l1.price - 0.20).abs() < 1e-6, "expected level1 ~0.20");
    }

    #[test]
    fn skew_repair_pricing_removes_missing_side_when_cap_below_floor() {
        let mut state = make_state(300_000);
        state.inventory.up.shares = 1.0;
        state.inventory.up.notional_usdc = 0.95; // avg cost 0.95 => completion cap below 0.20
        state.inventory.down.shares = 0.0;
        state.down_book.best_ask = Some(0.30);
        state.down_book.tick_size = 0.01;

        let trading_cfg = TradingConfig::default();
        let inventory_cfg = InventoryConfig::default();
        let mut completion_cfg = CompletionConfig::default();
        completion_cfg.max_loss_usdc = 0.0;
        completion_cfg.min_profit_per_share = 0.0;

        let mut desired = vec![DesiredOrder {
            token_id: state.identity.token_down.clone(),
            level: 0,
            price: 0.25,
            size: 1.0,
            post_only: true,
            tif: TimeInForce::Gtc,
        }];

        apply_skew_repair_pricing(
            &mut desired,
            &state,
            &trading_cfg,
            &inventory_cfg,
            &completion_cfg,
            0,
        );

        assert!(
            desired.is_empty(),
            "expected missing-side orders to be removed when repair cap is below the quote floor"
        );
    }

    fn assert_taker_price_cap(avg_cost: f64, min_profit: f64, shares: f64) {
        let p_max =
            max_taker_price(avg_cost, min_profit, shares).expect("expected p_max to be computable");
        let target = 1.0 - avg_cost - min_profit;
        let spend = p_max + fee::taker_fee_usdc(shares, p_max) / shares;
        assert!(spend <= target + 1e-12, "expected spend <= target");
    }

    #[test]
    fn taker_price_cap_small_q() {
        assert_taker_price_cap(0.4, 0.01, 0.01);
    }

    #[test]
    fn taker_price_cap_large_q() {
        assert_taker_price_cap(0.4, 0.01, 10_000.0);
    }

    #[test]
    fn p_max_tick_rounds_down() {
        let raw = 0.503;
        let tick = 0.01;
        let rounded = round_down_to_tick(raw, tick).expect("rounded");
        assert!(rounded <= raw + 1e-12);
        assert!(((rounded / tick).round() - (rounded / tick)).abs() < 1e-9);
        assert!((rounded - 0.50).abs() < 1e-12);
    }

    #[test]
    fn should_taker_complete_rounds_pmax_to_tick() {
        let mut state = make_state(30_000);
        state.inventory.up.shares = 1.0;
        state.inventory.up.notional_usdc = 0.2;
        state.inventory.down.shares = 0.0;
        state.down_book.best_ask = Some(0.25);
        state.down_book.tick_size = 0.01;

        let mut inventory_cfg = InventoryConfig::default();
        inventory_cfg.taker_window_s = 30;

        let mut completion_cfg = CompletionConfig::default();
        completion_cfg.enabled = true;
        completion_cfg.order_type = CompletionOrderType::Fok;
        completion_cfg.min_profit_per_share = 0.0;
        completion_cfg.max_loss_usdc = 0.0;

        let raw = max_taker_price(0.2, 0.0, 1.0).expect("raw p_max");
        let action = should_taker_complete(
            &state,
            &inventory_cfg,
            &completion_cfg,
            &TradingConfig::default(),
            0,
        )
        .expect("expected taker completion");

        assert_eq!(action.side, Side::Buy);
        assert!(action.p_max <= raw + 1e-12);
        assert!(((action.p_max / 0.01).round() - (action.p_max / 0.01)).abs() < 1e-9);
    }

    #[test]
    fn skew_repair_removes_missing_side_when_post_only_cap_below_floor() {
        let mut state = make_state(10_000_000);
        // Excess = DOWN, missing = UP.
        state.inventory.down.shares = 0.1;
        state.inventory.down.notional_usdc = 0.02; // avg cost 0.2 => cap well above the floor
        state.inventory.up.shares = 0.0;
        state.inventory.up.notional_usdc = 0.0;

        state.up_book.tick_size = 0.01;
        // With min_ticks_from_ask=1, post-only max = 0.20 - 0.01 = 0.19 < floor (0.20).
        state.up_book.best_ask = Some(0.20);

        let mut desired = vec![
            DesiredOrder {
                token_id: state.identity.token_up.clone(),
                level: 0,
                price: 0.25,
                size: 1.0,
                post_only: false,
                tif: TimeInForce::Gtc,
            },
            DesiredOrder {
                token_id: state.identity.token_up.clone(),
                level: 1,
                price: 0.24,
                size: 1.0,
                post_only: false,
                tif: TimeInForce::Gtc,
            },
        ];

        let trading_cfg = TradingConfig::default();
        let inventory_cfg = InventoryConfig::default();
        let mut completion_cfg = CompletionConfig::default();
        completion_cfg.max_loss_usdc = 0.0;

        apply_skew_repair_pricing(
            &mut desired,
            &state,
            &trading_cfg,
            &inventory_cfg,
            &completion_cfg,
            0,
        );

        assert!(
            desired.is_empty(),
            "expected missing-side orders to be removed when max_post_only < trading.min_quote_price"
        );
    }
}
