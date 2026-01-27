#![allow(dead_code)]

use serde_json::json;
use tokio::sync::{mpsc::{Receiver, Sender}, watch};

use crate::config::{InventoryConfig, RewardsConfig, TradingConfig};
use crate::persistence::LogEvent;
use crate::state::market_state::MarketState;
use crate::state::state_manager::QuoteTick;
use crate::strategy::quote_engine::build_desired_orders;
use crate::strategy::reward_engine::{apply_reward_hints, RewardApplySummary, RewardsSnapshot};
use crate::strategy::risk::adjust_for_inventory;
use crate::strategy::DesiredOrder;

#[derive(Debug, Clone)]
pub struct ExecCommand {
    pub slug: String,
    pub desired: Vec<DesiredOrder>,
}

#[derive(Debug, Clone)]
pub struct StrategyEngine {
    trading: TradingConfig,
    inventory: InventoryConfig,
    rewards: RewardsConfig,
    rewards_rx: Option<watch::Receiver<RewardsSnapshot>>,
}

impl StrategyEngine {
    pub fn new(
        trading: TradingConfig,
        inventory: InventoryConfig,
        rewards: RewardsConfig,
        rewards_rx: Option<watch::Receiver<RewardsSnapshot>>,
    ) -> Self {
        Self {
            trading,
            inventory,
            rewards,
            rewards_rx,
        }
    }

    pub async fn run(self, rx_quote: Receiver<QuoteTick>, tx_exec: Sender<ExecCommand>) {
        self.run_with_logger(rx_quote, tx_exec, None).await
    }

    pub async fn run_with_logger(
        self,
        mut rx_quote: Receiver<QuoteTick>,
        tx_exec: Sender<ExecCommand>,
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

            log_desired_summary(&slug, &state, &desired);
            log_desired_orders(&log_tx, &slug, now_ms, &desired);
            log_reward_summary(&log_tx, &slug, now_ms, &reward_summary);

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

        let engine = StrategyEngine::new(
            TradingConfig::default(),
            InventoryConfig::default(),
            RewardsConfig::default(),
            None,
        );
        let handle = tokio::spawn(engine.run(rx_quote, tx_exec));

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
}
