#![allow(dead_code)]

use serde_json::json;
use tokio::sync::{mpsc::{Receiver, Sender}, watch};

use crate::config::{CompletionConfig, InventoryConfig, RewardsConfig, TradingConfig};
use crate::persistence::LogEvent;
use crate::state::market_state::MarketState;
use crate::state::state_manager::QuoteTick;
use crate::strategy::fee;
use crate::strategy::quote_engine::build_desired_orders;
use crate::strategy::reward_engine::{apply_reward_hints, RewardApplySummary, RewardsSnapshot};
use crate::strategy::risk::{adjust_for_inventory, should_taker_complete};
use crate::strategy::DesiredOrder;

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
        self,
        mut rx_quote: Receiver<QuoteTick>,
        tx_exec: Sender<ExecCommand>,
        tx_completion: Sender<CompletionCommand>,
        log_tx: Option<Sender<LogEvent>>,
    ) {
        while let Some(tick) = rx_quote.recv().await {
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

            let mut desired = build_desired_orders(&state, &self.trading, now_ms);
            adjust_for_inventory(&mut desired, &state, &self.inventory, now_ms);

            let cap_usdc = self.trading.max_usdc_exposure_per_market;
            let inv_cost_usdc =
                (state.inventory.up.notional_usdc + state.inventory.down.notional_usdc).max(0.0);
            let remaining_budget_usdc = if cap_usdc.is_finite() && cap_usdc > 0.0 {
                (cap_usdc - inv_cost_usdc).max(0.0)
            } else {
                f64::INFINITY
            };

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

            if let Some(taker_action) =
                should_taker_complete(&state, &self.inventory, &self.completion, now_ms)
            {
                let completion_cost = taker_action.shares * taker_action.p_max
                    + fee::taker_fee_usdc(taker_action.shares, taker_action.p_max);
                if completion_cost > remaining_budget_usdc + 1e-12 {
                    tracing::warn!(
                        target: "strategy_engine",
                        slug = %slug,
                        token_id = %taker_action.token_id,
                        completion_cost_usdc = completion_cost,
                        remaining_budget_usdc,
                        "taker completion suppressed by market USDC budget"
                    );
                } else {
                tracing::info!(
                    target: "strategy_engine",
                    slug = %slug,
                    token_id = %taker_action.token_id,
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

            let reward_snapshot = self
                .rewards_rx
                .as_ref()
                .map(|rx| rx.borrow().clone());
            let reward_summary = apply_reward_hints(
                &mut desired,
                &state,
                &self.trading,
                &self.rewards,
                reward_snapshot.as_ref(),
            );

            let budget_pruned = enforce_market_budget(&mut desired, &state, &self.trading);

            log_desired_summary(&slug, &state, &desired);
            log_desired_orders(&log_tx, &slug, now_ms, &desired);
            log_reward_summary(&log_tx, &slug, now_ms, &reward_summary);
            if budget_pruned {
                log_budget_prune(&log_tx, &slug, now_ms, &state, &desired, &self.trading);
            }

            if tx_exec
                .send(ExecCommand {
                    slug,
                    desired,
                })
                .await
                .is_err()
            {
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

fn log_desired_summary(slug: &str, state: &MarketState, desired: &[DesiredOrder]) {
    let top_up = top_level_order(desired, &state.identity.token_up);
    let top_down = top_level_order(desired, &state.identity.token_down);

    tracing::info!(
        target: "strategy_engine",
        slug = %slug,
        desired_count = desired.len(),
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
    let payload = json!({
        "slug": slug,
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
    async fn emits_completion_only_within_window_and_below_pmax() {
        let (tx_quote, rx_quote) = mpsc::channel(1);
        // Buffer exec commands so the strategy task never blocks in this test.
        let (tx_exec, _rx_exec) = mpsc::channel(16);
        let (tx_completion, mut rx_completion) = mpsc::channel(1);

        let mut completion_cfg = CompletionConfig::default();
        completion_cfg.enabled = true;

        let engine = StrategyEngine::new(
            TradingConfig::default(),
            InventoryConfig::default(),
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
        state_inside.down_book.best_ask = Some(0.1);

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
        assert!(cmd.p_max >= 0.1);

        let mut state_outside = MarketState::new(identity.clone(), now_ms + 60_000);
        state_outside.inventory.up.shares = 1.0;
        state_outside.inventory.up.notional_usdc = 0.2;
        state_outside.inventory.down.shares = 0.0;
        state_outside.down_book.best_ask = Some(0.1);

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

        let mut state_above = MarketState::new(identity, now_ms + 30_000);
        state_above.inventory.up.shares = 1.0;
        state_above.inventory.up.notional_usdc = 0.9;
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
}
