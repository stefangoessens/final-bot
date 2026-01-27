#![allow(dead_code)]

use crate::config::{CompletionConfig, CompletionOrderType, InventoryConfig};
use crate::state::market_state::MarketState;
use crate::strategy::fee;
use crate::strategy::DesiredOrder;

#[derive(Debug, Clone)]
pub struct TakerAction {
    pub token_id: String,
    pub p_max: f64,
    pub shares: f64,
    pub order_type: CompletionOrderType,
}

pub fn adjust_for_inventory(
    desired: &mut Vec<DesiredOrder>,
    state: &MarketState,
    cfg: &InventoryConfig,
    now_ms: i64,
) {
    if desired.is_empty() {
        return;
    }

    let up_shares = state.inventory.up.shares;
    let down_shares = state.inventory.down.shares;
    let unpaired = (up_shares - down_shares).abs();
    if unpaired <= 0.0 {
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

    let time_to_cancel_ms = state.cutoff_ts_ms - now_ms;
    let time_to_cancel_s = time_to_cancel_ms / 1000;
    let hard_cap = cfg
        .max_unpaired_shares_per_market
        .min(cfg.max_unpaired_shares_global);
    let emergency = unpaired > cfg.skew_severe
        || time_to_cancel_s <= cfg.emergency_window_s
        || unpaired >= hard_cap;
    let moderate = unpaired > cfg.skew_moderate;

    let (missing_mult, excess_mult) = if unpaired > cfg.skew_mild {
        let denom = cfg.skew_moderate - cfg.skew_mild;
        if denom > 0.0 {
            let ratio = ((unpaired - cfg.skew_mild) / denom).clamp(0.0, 1.0);
            let missing_mult = 1.0 + ratio;
            (missing_mult, 1.0 / missing_mult)
        } else {
            (1.0, 1.0)
        }
    } else {
        (1.0, 1.0)
    };

    for order in desired.iter_mut() {
        if order.token_id == missing_token {
            order.size *= missing_mult;
        } else if order.token_id == excess_token {
            order.size *= excess_mult;
        }
    }

    if emergency {
        desired.retain(|order| order.token_id != excess_token);
    } else if moderate {
        desired.retain(|order| !(order.token_id == excess_token && order.level == 0));
    }
}

pub fn should_taker_complete(
    state: &MarketState,
    inventory_cfg: &InventoryConfig,
    completion_cfg: &CompletionConfig,
    now_ms: i64,
) -> Option<TakerAction> {
    if !completion_cfg.enabled || !completion_cfg.use_explicit_price_cap {
        return None;
    }

    let up_shares = state.inventory.up.shares;
    let down_shares = state.inventory.down.shares;
    let unpaired = (up_shares - down_shares).abs();
    if unpaired <= 0.0 {
        return None;
    }

    let time_to_cancel_ms = state.cutoff_ts_ms - now_ms;
    let time_to_cancel_s = time_to_cancel_ms / 1000;
    if time_to_cancel_s > inventory_cfg.taker_window_s {
        return None;
    }

    let (missing_token, missing_ask, excess_avg_cost) = if up_shares < down_shares {
        (
            state.identity.token_up.as_str(),
            state.up_book.best_ask,
            state.inventory.down.avg_cost(),
        )
    } else {
        (
            state.identity.token_down.as_str(),
            state.down_book.best_ask,
            state.inventory.up.avg_cost(),
        )
    };

    let avg_cost = excess_avg_cost?;
    let min_profit = completion_cfg.min_profit_per_share;
    let emergency_loss_per_share = if completion_cfg.max_loss_usdc > 0.0 && unpaired > 0.0 {
        completion_cfg.max_loss_usdc / unpaired
    } else {
        0.0
    };
    let min_profit = min_profit.max(-emergency_loss_per_share);

    let p_max = max_taker_price(avg_cost, min_profit)?;
    let best_ask = missing_ask?;
    if best_ask > p_max {
        return None;
    }

    Some(TakerAction {
        token_id: missing_token.to_string(),
        p_max,
        shares: unpaired,
        order_type: completion_cfg.order_type,
    })
}

fn max_taker_price(avg_cost: f64, min_profit: f64) -> Option<f64> {
    if !avg_cost.is_finite() || !min_profit.is_finite() {
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
        let spend = mid + fee::taker_fee_per_share(mid);
        if spend > target {
            hi = mid;
        } else {
            lo = mid;
        }
    }

    Some(lo)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CompletionConfig;
    use crate::state::market_state::{MarketIdentity, MarketState};
    use crate::strategy::TimeInForce;

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
    fn skew_moderate_removes_excess_level0() {
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
            !desired.iter().any(|o| o.token_id == "UP" && o.level == 0),
            "expected excess side level0 to be removed"
        );
        assert!(
            desired.iter().any(|o| o.token_id == "UP" && o.level == 1),
            "expected excess side deeper level to remain"
        );
        assert!(
            desired.iter().any(|o| o.token_id == "DOWN" && o.level == 0),
            "expected missing side level0 to remain"
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

        let none_action = should_taker_complete(&state, &inventory_cfg, &completion_cfg, 0);
        assert!(none_action.is_none(), "expected no taker completion");

        state.inventory.up.notional_usdc = 0.2; // avg cost 0.2
        state.down_book.best_ask = Some(0.1);

        let some_action =
            should_taker_complete(&state, &inventory_cfg, &completion_cfg, 0)
                .expect("expected taker completion");
        assert!(
            some_action.p_max >= state.down_book.best_ask.unwrap(),
            "expected p_max >= best ask"
        );
        assert_eq!(some_action.order_type, CompletionOrderType::Fok);
    }

    #[test]
    fn taker_completion_blocks_if_ask_above_p_max() {
        let mut state = make_state(30_000);
        state.inventory.up.shares = 1.0;
        state.inventory.up.notional_usdc = 0.8; // avg cost 0.8
        state.inventory.down.shares = 0.0;

        let mut inventory_cfg = InventoryConfig::default();
        inventory_cfg.taker_window_s = 30;
        let mut completion_cfg = CompletionConfig::default();
        completion_cfg.enabled = true;
        completion_cfg.order_type = CompletionOrderType::Fok;
        completion_cfg.min_profit_per_share = 0.01;
        completion_cfg.max_loss_usdc = 0.0;

        let p_max = max_taker_price(0.8, 0.01).expect("p_max should be computable");
        state.down_book.best_ask = Some(p_max + 0.05);

        let action = should_taker_complete(&state, &inventory_cfg, &completion_cfg, 0);
        assert!(action.is_none(), "expected completion to be blocked");
    }
}
