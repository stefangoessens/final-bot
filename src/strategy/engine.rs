#![allow(dead_code)]

use serde_json::json;
use tokio::sync::{
    mpsc::error::TryRecvError,
    mpsc::{Receiver, Sender},
    watch,
};

use crate::config::{CompletionConfig, InventoryConfig, RewardsConfig, TradingConfig};
use crate::persistence::LogEvent;
use crate::state::market_state::MarketState;
use crate::state::state_manager::QuoteTick;
use crate::strategy::fee;
use crate::strategy::quote_engine::{build_desired_orders, Level0ChaseLimiter};
use crate::strategy::reward_engine::{apply_reward_hints, RewardApplySummary, RewardsSnapshot};
use crate::strategy::risk::{
    adjust_for_inventory, apply_skew_repair_pricing, should_taker_complete,
};
use crate::strategy::DesiredOrder;
use polymarket_client_sdk::clob::types::Side;

const UNPAIRED_EPS: f64 = 1e-6;

#[derive(Debug, Clone)]
pub struct ExecCommand {
    pub slug: String,
    pub desired: Vec<DesiredOrder>,
}

#[derive(Debug, Clone)]
pub struct CompletionCommand {
    pub slug: String,
    pub condition_id: String,
    pub token_id: String,
    pub cancel_order_ids: Vec<String>,
    pub side: Side,
    pub shares: f64,
    pub p_max: f64,
    pub order_type: crate::config::CompletionOrderType,
    pub now_ms: i64,
}

#[derive(Debug, Clone)]
pub struct StrategyEngine {
    trading: TradingConfig,
    inventory: InventoryConfig,
    completion: CompletionConfig,
    rewards: RewardsConfig,
    rewards_rx: Option<watch::Receiver<RewardsSnapshot>>,
    chase: Level0ChaseLimiter,
}

impl StrategyEngine {
    pub fn new(
        trading: TradingConfig,
        inventory: InventoryConfig,
        completion: CompletionConfig,
        rewards: RewardsConfig,
        rewards_rx: Option<watch::Receiver<RewardsSnapshot>>,
    ) -> Self {
        Self {
            trading,
            inventory,
            completion,
            rewards,
            rewards_rx,
            chase: Level0ChaseLimiter::default(),
        }
    }

    pub async fn run(
        self,
        rx_quote: Receiver<QuoteTick>,
        tx_exec: Sender<ExecCommand>,
        tx_completion: Sender<CompletionCommand>,
    ) {
        self.run_with_logger(rx_quote, tx_exec, tx_completion, None)
            .await
    }

    pub async fn run_with_logger(
        mut self,
        mut rx_quote: Receiver<QuoteTick>,
        tx_exec: Sender<ExecCommand>,
        tx_completion: Sender<CompletionCommand>,
        log_tx: Option<Sender<LogEvent>>,
    ) {
        while let Some(mut tick) = rx_quote.recv().await {
            // Drain backlog and only process the most recent tick.
            loop {
                match rx_quote.try_recv() {
                    Ok(next) => tick = next,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }

            let QuoteTick {
                slug,
                now_ms,
                state,
            } = tick;

            if !state.quoting_enabled {
                tracing::info!(
                    target: "strategy_engine",
                    slug = %slug,
                    desired_count = 0,
                    "quoting disabled; sending empty desired orders"
                );
                if tx_exec
                    .send(ExecCommand {
                        slug,
                        desired: Vec::new(),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
                continue;
            }

            let chase = if self.trading.chase_limiter_enabled {
                Some(&mut self.chase)
            } else {
                None
            };
            let mut desired = build_desired_orders(&state, &self.trading, now_ms, chase);
            adjust_for_inventory(&mut desired, &state, &self.inventory, now_ms);
            apply_skew_repair_pricing(
                &mut desired,
                &state,
                &self.trading,
                &self.inventory,
                &self.completion,
                now_ms,
            );

            let cap_usdc = self.trading.max_usdc_exposure_per_market;
            let inv_cost_usdc =
                (state.inventory.up.notional_usdc + state.inventory.down.notional_usdc).max(0.0);
            let remaining_budget_usdc = if cap_usdc.is_finite() && cap_usdc > 0.0 {
                (cap_usdc - inv_cost_usdc).max(0.0)
            } else {
                f64::INFINITY
            };

            if let Some(taker_action) = should_taker_complete(
                &state,
                &self.inventory,
                &self.completion,
                &self.trading,
                now_ms,
            ) {
                let completion_cost = if matches!(taker_action.side, Side::Sell) {
                    0.0
                } else {
                    taker_action.shares * taker_action.p_max
                        + fee::taker_fee_usdc(taker_action.shares, taker_action.p_max)
                };
                if completion_cost > remaining_budget_usdc + 1e-12 {
                    tracing::warn!(
                        target: "strategy_engine",
                        slug = %slug,
                        token_id = %taker_action.token_id,
                        side = ?taker_action.side,
                        completion_cost_usdc = completion_cost,
                        remaining_budget_usdc,
                        "taker completion suppressed by market USDC budget"
                    );
                } else {
                    tracing::info!(
                        target: "strategy_engine",
                        slug = %slug,
                        token_id = %taker_action.token_id,
                        side = ?taker_action.side,
                        p_max = taker_action.p_max,
                        shares = taker_action.shares,
                        order_type = ?taker_action.order_type,
                        "taker completion triggered"
                    );

                    if tx_exec
                        .send(ExecCommand {
                            slug: slug.clone(),
                            desired: Vec::new(),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }

                    if tx_completion
                        .send(CompletionCommand {
                            slug: slug.clone(),
                            condition_id: state.identity.condition_id.clone(),
                            token_id: taker_action.token_id.clone(),
                            cancel_order_ids: state
                                .orders
                                .live
                                .values()
                                .filter(|order| {
                                    order.remaining.is_finite() && order.remaining > 0.0
                                })
                                .map(|order| order.order_id.clone())
                                .collect(),
                            side: taker_action.side,
                            shares: taker_action.shares,
                            p_max: taker_action.p_max,
                            order_type: taker_action.order_type,
                            now_ms,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }

                    log_completion_command(&log_tx, &slug, now_ms, &taker_action);
                    continue;
                }
            }

            if remaining_budget_usdc <= 0.0 {
                tracing::warn!(
                    target: "strategy_engine",
                    slug = %slug,
                    cap_usdc = cap_usdc,
                    inventory_cost_usdc = inv_cost_usdc,
                    "market USDC budget exhausted; sending empty desired orders"
                );
                if tx_exec
                    .send(ExecCommand {
                        slug,
                        desired: Vec::new(),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
                continue;
            }

            let reward_snapshot = self.rewards_rx.as_ref().map(|rx| rx.borrow().clone());
            let reward_summary = apply_reward_hints(
                &mut desired,
                &state,
                &self.trading,
                &self.rewards,
                reward_snapshot.as_ref(),
            );

            let budget_pruned = enforce_market_budget(&mut desired, &state, &self.trading);
            let safety_pruned = enforce_final_safety(&mut desired, &state, &self.trading);

            log_desired_summary(&slug, &state, &desired);
            log_desired_orders(&log_tx, &slug, now_ms, &state, &desired);
            log_reward_summary(&log_tx, &slug, now_ms, &reward_summary);
            if budget_pruned {
                log_budget_prune(&log_tx, &slug, now_ms, &state, &desired, &self.trading);
            }
            if safety_pruned {
                tracing::warn!(
                    target: "strategy_engine",
                    slug = %slug,
                    desired_count = desired.len(),
                    "desired orders pruned by final safety constraints"
                );
            }

            if tx_exec.send(ExecCommand { slug, desired }).await.is_err() {
                break;
            }
        }
    }
}

fn enforce_market_budget(
    desired: &mut Vec<DesiredOrder>,
    state: &MarketState,
    cfg: &TradingConfig,
) -> bool {
    let cap = cfg.max_usdc_exposure_per_market;
    if !cap.is_finite() || cap <= 0.0 {
        return false;
    }

    let inv_cost = (state.inventory.up.notional_usdc + state.inventory.down.notional_usdc).max(0.0);
    if inv_cost >= cap - 1e-12 {
        if !desired.is_empty() {
            desired.clear();
            return true;
        }
        return false;
    }

    let budget_open = (cap - inv_cost).max(0.0);
    let mut total_open: f64 = desired
        .iter()
        .map(|o| o.price.max(0.0) * o.size.max(0.0))
        .sum();

    if total_open <= budget_open + 1e-12 {
        return false;
    }

    // Prune the least important orders until within budget:
    // 1) deepest level first (highest `level`)
    // 2) within same level, largest notional first (price*size)
    let mut pruned = false;
    while total_open > budget_open + 1e-12 && !desired.is_empty() {
        let mut worst_idx = 0usize;
        let mut worst_key: (usize, f64) = (0, 0.0);
        for (idx, order) in desired.iter().enumerate() {
            let notional = order.price.max(0.0) * order.size.max(0.0);
            let key = (order.level, notional);
            if key > worst_key {
                worst_key = key;
                worst_idx = idx;
            }
        }
        desired.swap_remove(worst_idx);
        pruned = true;
        total_open = desired
            .iter()
            .map(|o| o.price.max(0.0) * o.size.max(0.0))
            .sum();
    }

    pruned
}

fn enforce_final_safety(
    desired: &mut Vec<DesiredOrder>,
    state: &MarketState,
    cfg: &TradingConfig,
) -> bool {
    if desired.is_empty() {
        return false;
    }

    let before = desired.len();

    let up_token = state.identity.token_up.as_str();
    let down_token = state.identity.token_down.as_str();
    desired.retain(|o| {
        (o.token_id == up_token || o.token_id == down_token)
            && o.price.is_finite()
            && o.size.is_finite()
            && o.size > 0.0
            && o.price >= cfg.min_quote_price
    });
    if desired.is_empty() {
        return before != 0;
    }

    let up_shares = state.inventory.up.shares;
    let down_shares = state.inventory.down.shares;
    let unpaired = (up_shares - down_shares).abs();
    if unpaired <= UNPAIRED_EPS {
        let has_up = desired
            .iter()
            .any(|o| o.token_id == state.identity.token_up);
        let has_down = desired
            .iter()
            .any(|o| o.token_id == state.identity.token_down);
        if has_up ^ has_down {
            desired.clear();
        }
    } else {
        let excess = if up_shares > down_shares {
            state.identity.token_up.as_str()
        } else {
            state.identity.token_down.as_str()
        };
        desired.retain(|o| o.token_id != excess);
    }

    desired.len() != before
}

fn log_desired_summary(slug: &str, state: &MarketState, desired: &[DesiredOrder]) {
    let top_up = top_level_order(desired, &state.identity.token_up);
    let top_down = top_level_order(desired, &state.identity.token_down);

    let level0_total = match (top_up.map(|o| o.price), top_down.map(|o| o.price)) {
        (Some(u), Some(d)) => Some(u + d),
        _ => None,
    };

    tracing::info!(
        target: "strategy_engine",
        slug = %slug,
        desired_count = desired.len(),
        target_total = state.alpha.target_total,
        level0_total = ?level0_total,
        level0_edge = ?level0_total.map(|x| 1.0 - x),
        vol_ratio = state.alpha.vol_ratio,
        spread_ratio = state.alpha.spread_ratio,
        fast_move = state.alpha.fast_move,
        oracle_disagree = state.alpha.oracle_disagree,
        binance_stale = state.alpha.binance_stale,
        up_level = ?top_up.map(|o| o.level),
        up_price = ?top_up.map(|o| o.price),
        up_size = ?top_up.map(|o| o.size),
        down_level = ?top_down.map(|o| o.level),
        down_price = ?top_down.map(|o| o.price),
        down_size = ?top_down.map(|o| o.size),
        "desired orders"
    );
}

fn log_budget_prune(
    log_tx: &Option<Sender<LogEvent>>,
    slug: &str,
    ts_ms: i64,
    state: &MarketState,
    desired: &[DesiredOrder],
    cfg: &TradingConfig,
) {
    let Some(tx) = log_tx else {
        return;
    };

    let inv_cost = (state.inventory.up.notional_usdc + state.inventory.down.notional_usdc).max(0.0);
    let open_cost: f64 = desired
        .iter()
        .map(|o| o.price.max(0.0) * o.size.max(0.0))
        .sum();

    let payload = json!({
        "slug": slug,
        "cap_usdc": cfg.max_usdc_exposure_per_market,
        "inventory_cost_usdc": inv_cost,
        "desired_open_cost_usdc": open_cost,
        "desired_count": desired.len(),
    });
    let _ = tx.try_send(LogEvent {
        ts_ms,
        event: "risk.market_budget_prune".to_string(),
        payload,
    });
}

fn top_level_order<'a>(desired: &'a [DesiredOrder], token_id: &str) -> Option<&'a DesiredOrder> {
    desired
        .iter()
        .filter(|order| order.token_id == token_id)
        .min_by_key(|order| order.level)
}

fn log_desired_orders(
    log_tx: &Option<Sender<LogEvent>>,
    slug: &str,
    ts_ms: i64,
    state: &MarketState,
    desired: &[DesiredOrder],
) {
    let Some(tx) = log_tx else {
        return;
    };
    let orders: Vec<_> = desired
        .iter()
        .map(|order| {
            json!({
                "token_id": order.token_id,
                "level": order.level,
                "price": order.price,
                "size": order.size,
                "post_only": order.post_only,
            })
        })
        .collect();

    let top_up = top_level_order(desired, &state.identity.token_up);
    let top_down = top_level_order(desired, &state.identity.token_down);
    let level0_total = match (top_up.map(|o| o.price), top_down.map(|o| o.price)) {
        (Some(u), Some(d)) => Some(u + d),
        _ => None,
    };

    let payload = json!({
        "slug": slug,
        "target_total": state.alpha.target_total,
        "level0_total": level0_total,
        "level0_edge": level0_total.map(|x| 1.0 - x),
        "vol_ratio": state.alpha.vol_ratio,
        "spread_ratio": state.alpha.spread_ratio,
        "fast_move": state.alpha.fast_move,
        "oracle_disagree": state.alpha.oracle_disagree,
        "binance_stale": state.alpha.binance_stale,
        "up_shares": state.inventory.up.shares,
        "down_shares": state.inventory.down.shares,
        "count": desired.len(),
        "orders": orders,
    });
    let _ = tx.try_send(LogEvent {
        ts_ms,
        event: "strategy.desired".to_string(),
        payload,
    });
}

fn log_reward_summary(
    log_tx: &Option<Sender<LogEvent>>,
    slug: &str,
    ts_ms: i64,
    summary: &RewardApplySummary,
) {
    if !(summary.up_improved
        || summary.down_improved
        || summary.up_size_bumped
        || summary.down_size_bumped)
    {
        return;
    }
    let Some(tx) = log_tx else {
        return;
    };

    let payload = json!({
        "slug": slug,
        "up_scoring": summary.up_scoring,
        "down_scoring": summary.down_scoring,
        "up_improved": summary.up_improved,
        "down_improved": summary.down_improved,
        "up_size_bumped": summary.up_size_bumped,
        "down_size_bumped": summary.down_size_bumped,
    });
    let _ = tx.try_send(LogEvent {
        ts_ms,
        event: "strategy.rewards_apply".to_string(),
        payload,
    });
}

fn log_completion_command(
    log_tx: &Option<Sender<LogEvent>>,
    slug: &str,
    ts_ms: i64,
    taker_action: &crate::strategy::risk::TakerAction,
) {
    let Some(tx) = log_tx else {
        return;
    };
    let payload = json!({
        "slug": slug,
        "token_id": taker_action.token_id,
        "side": format!("{:?}", taker_action.side),
        "shares": taker_action.shares,
        "p_max": taker_action.p_max,
        "order_type": format!("{:?}", taker_action.order_type),
    });
    let _ = tx.try_send(LogEvent {
        ts_ms,
        event: "strategy.completion".to_string(),
        payload,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;
    use tokio::time::{timeout, Duration};

    use crate::state::market_state::MarketIdentity;
    use crate::state::market_state::MarketState;
    use crate::strategy::TimeInForce;

    #[tokio::test]
    async fn emits_exec_command_on_quote_tick() {
        let (tx_quote, rx_quote) = mpsc::channel(1);
        let (tx_exec, mut rx_exec) = mpsc::channel(1);
        let (tx_completion, _rx_completion) = mpsc::channel(1);

        let engine = StrategyEngine::new(
            TradingConfig::default(),
            InventoryConfig::default(),
            CompletionConfig::default(),
            RewardsConfig::default(),
            None,
        );
        let handle = tokio::spawn(engine.run(rx_quote, tx_exec, tx_completion));

        let identity = MarketIdentity {
            slug: "btc-15m-test".to_string(),
            interval_start_ts: 1_700_000_000,
            interval_end_ts: 1_700_000_900,
            condition_id: "cond".to_string(),
            token_up: "token_up".to_string(),
            token_down: "token_down".to_string(),
            active: true,
            closed: false,
            accepting_orders: true,
            restricted: false,
        };
        let state = MarketState::new(identity, 0);

        tx_quote
            .send(QuoteTick {
                slug: "btc-15m-test".to_string(),
                now_ms: 1_700_000_100_000,
                state,
            })
            .await
            .expect("send quote tick");
        drop(tx_quote);

        let msg = timeout(Duration::from_millis(200), rx_exec.recv())
            .await
            .expect("exec command timeout");
        assert!(msg.is_some());

        let _ = handle.await;
    }

    #[tokio::test]
    async fn emits_completion_within_window_or_when_hardcap_breached() {
        let (tx_quote, rx_quote) = mpsc::channel(1);
        // Buffer exec commands so the strategy task never blocks in this test.
        let (tx_exec, _rx_exec) = mpsc::channel(16);
        let (tx_completion, mut rx_completion) = mpsc::channel(1);

        let mut completion_cfg = CompletionConfig::default();
        completion_cfg.enabled = true;
        completion_cfg.max_loss_usdc = 0.0;

        let mut inventory_cfg = InventoryConfig::default();
        inventory_cfg.max_unpaired_shares_per_market = 1.0;
        inventory_cfg.max_unpaired_shares_global = 1.0;
        inventory_cfg.taker_window_s = 30;

        let engine = StrategyEngine::new(
            TradingConfig::default(),
            inventory_cfg,
            completion_cfg,
            RewardsConfig::default(),
            None,
        );
        let handle = tokio::spawn(engine.run(rx_quote, tx_exec, tx_completion));

        let identity = MarketIdentity {
            slug: "btc-15m-test".to_string(),
            interval_start_ts: 1_700_000_000,
            interval_end_ts: 1_700_000_900,
            condition_id: "cond".to_string(),
            token_up: "token_up".to_string(),
            token_down: "token_down".to_string(),
            active: true,
            closed: false,
            accepting_orders: true,
            restricted: false,
        };

        let now_ms = 1_700_000_100_000;

        let mut state_inside = MarketState::new(identity.clone(), now_ms + 30_000);
        state_inside.inventory.up.shares = 1.0;
        state_inside.inventory.up.notional_usdc = 0.2;
        state_inside.inventory.down.shares = 0.0;
        state_inside.down_book.best_ask = Some(0.25);

        tx_quote
            .send(QuoteTick {
                slug: identity.slug.clone(),
                now_ms,
                state: state_inside,
            })
            .await
            .expect("send quote tick");

        let msg = timeout(Duration::from_millis(200), rx_completion.recv())
            .await
            .expect("completion command timeout");
        let cmd = msg.expect("expected completion command");
        assert_eq!(cmd.condition_id, identity.condition_id);
        assert_eq!(cmd.token_id, identity.token_down);
        assert_eq!(cmd.side, Side::Buy);
        assert!(cmd.p_max >= 0.25);

        let mut state_outside = MarketState::new(identity.clone(), now_ms + 60_000);
        state_outside.inventory.up.shares = 0.25;
        state_outside.inventory.up.notional_usdc = 0.05;
        state_outside.inventory.down.shares = 0.0;
        state_outside.down_book.best_ask = Some(0.25);

        tx_quote
            .send(QuoteTick {
                slug: identity.slug.clone(),
                now_ms,
                state: state_outside,
            })
            .await
            .expect("send quote tick outside window");

        let suppressed = timeout(Duration::from_millis(100), rx_completion.recv()).await;
        assert!(suppressed.is_err(), "expected no completion outside window");

        let mut state_hardcap = MarketState::new(identity.clone(), now_ms + 60_000);
        state_hardcap.inventory.up.shares = 1.0;
        state_hardcap.inventory.up.notional_usdc = 0.2;
        state_hardcap.inventory.down.shares = 0.0;
        state_hardcap.down_book.best_ask = Some(0.25);

        tx_quote
            .send(QuoteTick {
                slug: identity.slug.clone(),
                now_ms,
                state: state_hardcap,
            })
            .await
            .expect("send quote tick hardcap outside window");

        let msg = timeout(Duration::from_millis(200), rx_completion.recv())
            .await
            .expect("completion command timeout");
        assert!(
            msg.is_some(),
            "expected completion when hardcap breached outside window"
        );

        let mut state_above = MarketState::new(identity, now_ms + 30_000);
        state_above.inventory.up.shares = 1.0;
        state_above.inventory.up.notional_usdc = 0.6;
        state_above.inventory.down.shares = 0.0;
        state_above.down_book.best_ask = Some(0.9);

        tx_quote
            .send(QuoteTick {
                slug: "btc-15m-test".to_string(),
                now_ms,
                state: state_above,
            })
            .await
            .expect("send quote tick above pmax");

        let suppressed = timeout(Duration::from_millis(100), rx_completion.recv()).await;
        assert!(
            suppressed.is_err(),
            "expected no completion when ask above p_max"
        );

        drop(tx_quote);
        let _ = handle.await;
    }

    fn make_state_for_safety() -> MarketState {
        let identity = MarketIdentity {
            slug: "btc-15m-safety".to_string(),
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
        MarketState::new(identity, 10_000_000)
    }

    #[test]
    fn final_safety_clears_one_sided_when_paired() {
        let state = make_state_for_safety();
        let cfg = TradingConfig::default();
        let mut desired = vec![DesiredOrder {
            token_id: state.identity.token_up.clone(),
            side: crate::state::state_manager::OrderSide::Buy,
            level: 0,
            price: 0.25,
            size: 1.0,
            post_only: true,
            tif: TimeInForce::Gtc,
        }];

        let pruned = enforce_final_safety(&mut desired, &state, &cfg);
        assert!(pruned, "expected safety prune to trigger");
        assert!(
            desired.is_empty(),
            "expected one-sided desired to be cleared"
        );
    }

    #[test]
    fn final_safety_enforces_missing_side_only_when_unpaired() {
        let mut state = make_state_for_safety();
        state.inventory.up.shares = 1.0;
        state.inventory.up.notional_usdc = 0.2;
        state.inventory.down.shares = 0.0;

        let cfg = TradingConfig::default();
        let mut desired = vec![
            DesiredOrder {
                token_id: state.identity.token_up.clone(),
                side: crate::state::state_manager::OrderSide::Buy,
                level: 0,
                price: 0.25,
                size: 1.0,
                post_only: true,
                tif: TimeInForce::Gtc,
            },
            DesiredOrder {
                token_id: state.identity.token_down.clone(),
                side: crate::state::state_manager::OrderSide::Buy,
                level: 0,
                price: 0.25,
                size: 1.0,
                post_only: true,
                tif: TimeInForce::Gtc,
            },
        ];

        let pruned = enforce_final_safety(&mut desired, &state, &cfg);
        assert!(pruned, "expected safety prune to trigger");
        assert!(
            desired
                .iter()
                .all(|o| o.token_id == state.identity.token_down),
            "expected only missing-side orders to remain"
        );
    }

    #[test]
    fn final_safety_removes_sub_floor_orders() {
        let state = make_state_for_safety();
        let cfg = TradingConfig::default(); // min_quote_price=0.20

        let mut desired = vec![
            DesiredOrder {
                token_id: state.identity.token_up.clone(),
                side: crate::state::state_manager::OrderSide::Buy,
                level: 0,
                price: cfg.min_quote_price - 0.01,
                size: 1.0,
                post_only: true,
                tif: TimeInForce::Gtc,
            },
            DesiredOrder {
                token_id: state.identity.token_down.clone(),
                side: crate::state::state_manager::OrderSide::Buy,
                level: 0,
                price: cfg.min_quote_price + 0.05,
                size: 1.0,
                post_only: true,
                tif: TimeInForce::Gtc,
            },
        ];

        let pruned = enforce_final_safety(&mut desired, &state, &cfg);
        assert!(pruned, "expected safety prune to trigger");
        assert!(
            desired.is_empty(),
            "expected paired inventory + sub-floor removal to clear one-sided desired"
        );
    }

    #[test]
    fn market_budget_prune_can_create_one_sided_and_final_safety_clears() {
        let state = make_state_for_safety();
        let mut cfg = TradingConfig::default();
        cfg.max_usdc_exposure_per_market = 0.60;

        let mut desired = vec![
            DesiredOrder {
                token_id: state.identity.token_up.clone(),
                side: crate::state::state_manager::OrderSide::Buy,
                level: 0,
                price: 0.49,
                size: 1.0,
                post_only: true,
                tif: TimeInForce::Gtc,
            },
            DesiredOrder {
                token_id: state.identity.token_down.clone(),
                side: crate::state::state_manager::OrderSide::Buy,
                level: 0,
                price: 0.49,
                size: 1.0,
                post_only: true,
                tif: TimeInForce::Gtc,
            },
        ];

        let budget_pruned = enforce_market_budget(&mut desired, &state, &cfg);
        assert!(budget_pruned, "expected market budget prune to trigger");
        assert_eq!(desired.len(), 1, "expected budget prune to leave one order");

        let safety_pruned = enforce_final_safety(&mut desired, &state, &cfg);
        assert!(safety_pruned, "expected final safety prune to trigger");
        assert!(
            desired.is_empty(),
            "expected one-sided desired to be cleared"
        );
    }

    #[test]
    fn alpha_cap_below_floor_results_in_no_quotes_when_unpaired() {
        let now_ms = 1_000;
        let mut state = make_state_for_safety();
        // Unpaired: excess=UP, missing=DOWN.
        state.inventory.up.shares = 1.0;
        state.inventory.up.notional_usdc = 0.3;
        state.inventory.down.shares = 0.0;

        state.up_book.best_bid = Some(0.50);
        state.up_book.best_ask = Some(0.51);
        state.up_book.tick_size = 0.01;
        state.down_book.best_bid = Some(0.50);
        state.down_book.best_ask = Some(0.51);
        state.down_book.tick_size = 0.01;

        let trading = TradingConfig::default(); // min_quote_price=0.20
        state.alpha.target_total = 0.99;
        state.alpha.cap_up = 0.99;
        // Missing side cap below floor => cannot quote missing side.
        state.alpha.cap_down = 0.19;
        state.alpha.size_scalar = 1.0;

        let mut desired =
            crate::strategy::quote_engine::build_desired_orders(&state, &trading, now_ms, None);
        assert!(
            desired
                .iter()
                .any(|o| o.token_id == state.identity.token_up),
            "expected quote_engine to produce excess-side orders before pairing enforcement"
        );
        assert!(
            !desired
                .iter()
                .any(|o| o.token_id == state.identity.token_down),
            "expected quote_engine to produce no missing-side orders when cap below floor"
        );

        crate::strategy::risk::adjust_for_inventory(
            &mut desired,
            &state,
            &InventoryConfig::default(),
            now_ms,
        );
        crate::strategy::risk::apply_skew_repair_pricing(
            &mut desired,
            &state,
            &trading,
            &InventoryConfig::default(),
            &CompletionConfig::default(),
            now_ms,
        );
        enforce_final_safety(&mut desired, &state, &trading);

        assert!(
            desired.is_empty(),
            "expected no maker quotes when unpaired and missing-side cap is below floor"
        );
    }
}
