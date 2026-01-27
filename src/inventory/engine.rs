#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time;

use crate::alpha::{toxicity, volatility};
use crate::config::{AlphaConfig, MergeConfig, OracleConfig};
use crate::inventory::onchain::OnchainRequest;
use crate::state::market_state::MarketState;
use crate::state::state_manager::QuoteTick;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventoryAction {
    Merge { condition_id: String, qty_sets: u64 },
    Redeem { condition_id: String },
    PauseMerges { reason: String },
}

#[derive(Debug, Clone)]
pub struct InventoryEngine {
    merge_cfg: MergeConfig,
    alpha_cfg: AlphaConfig,
    oracle_cfg: OracleConfig,
    last_merge_ms: HashMap<String, i64>,
    last_redeem_ms: HashMap<String, i64>,
    recent_ops: VecDeque<i64>,
}

impl InventoryEngine {
    pub fn new(merge_cfg: MergeConfig, alpha_cfg: AlphaConfig, oracle_cfg: OracleConfig) -> Self {
        Self {
            merge_cfg,
            alpha_cfg,
            oracle_cfg,
            last_merge_ms: HashMap::new(),
            last_redeem_ms: HashMap::new(),
            recent_ops: VecDeque::new(),
        }
    }

    pub fn tick(
        &mut self,
        now_ms: i64,
        market_states_snapshot: &[MarketState],
    ) -> Vec<InventoryAction> {
        if !self.merge_cfg.enabled || self.merge_cfg.batch_sets == 0 {
            return Vec::new();
        }

        self.prune_ops(now_ms);
        let mut actions = Vec::new();
        let mut pause_fast_move_emitted = false;
        let mut pause_interval_emitted = false;
        let mut pause_rate_limit_emitted = false;
        let min_sets = self.merge_cfg.min_sets;

        for market in market_states_snapshot {
            let full_sets = full_sets_u64(market.inventory.up.shares, market.inventory.down.shares);
            if full_sets < min_sets {
                continue;
            }

            if self.merge_cfg.pause_during_fast_move && self.is_fast_move(market, now_ms) {
                if !pause_fast_move_emitted {
                    actions.push(InventoryAction::PauseMerges {
                        reason: "fast_move".to_string(),
                    });
                    pause_fast_move_emitted = true;
                }
                continue;
            }

            match self.can_merge(&market.identity.condition_id, now_ms) {
                Ok(()) => {}
                Err(MergeBlockReason::IntervalBackoff) => {
                    if !pause_interval_emitted {
                        actions.push(InventoryAction::PauseMerges {
                            reason: "interval_backoff".to_string(),
                        });
                        pause_interval_emitted = true;
                    }
                    continue;
                }
                Err(MergeBlockReason::RateLimit) => {
                    if !pause_rate_limit_emitted {
                        actions.push(InventoryAction::PauseMerges {
                            reason: "rate_limit".to_string(),
                        });
                        pause_rate_limit_emitted = true;
                    }
                    continue;
                }
            }

            let merge_qty = (full_sets / self.merge_cfg.batch_sets) * self.merge_cfg.batch_sets;
            if merge_qty == 0 {
                continue;
            }

            actions.push(InventoryAction::Merge {
                condition_id: market.identity.condition_id.clone(),
                qty_sets: merge_qty,
            });
            self.record_merge(&market.identity.condition_id, now_ms);
        }

        for market in market_states_snapshot {
            if !market.identity.closed {
                continue;
            }
            if market.inventory.up.shares <= 0.0 && market.inventory.down.shares <= 0.0 {
                continue;
            }
            if self
                .can_redeem(&market.identity.condition_id, now_ms)
                .is_err()
            {
                continue;
            }

            actions.push(InventoryAction::Redeem {
                condition_id: market.identity.condition_id.clone(),
            });
            self.record_redeem(&market.identity.condition_id, now_ms);
        }

        actions
    }

    fn can_merge(&self, condition_id: &str, now_ms: i64) -> Result<(), MergeBlockReason> {
        let min_interval_ms = (self.merge_cfg.interval_s as i64).saturating_mul(1_000);
        if min_interval_ms > 0 {
            if let Some(last) = self.last_merge_ms.get(condition_id) {
                if now_ms.saturating_sub(*last) < min_interval_ms {
                    return Err(MergeBlockReason::IntervalBackoff);
                }
            }
        }

        let max_ops = self.merge_cfg.max_ops_per_minute;
        if max_ops > 0 && (self.recent_ops.len() as u64) >= max_ops {
            return Err(MergeBlockReason::RateLimit);
        }

        Ok(())
    }

    fn record_merge(&mut self, condition_id: &str, now_ms: i64) {
        self.last_merge_ms
            .insert(condition_id.to_string(), now_ms);
        self.recent_ops.push_back(now_ms);
    }

    fn can_redeem(&self, condition_id: &str, now_ms: i64) -> Result<(), MergeBlockReason> {
        let min_interval_ms = (self.merge_cfg.interval_s as i64).saturating_mul(1_000);
        if min_interval_ms > 0 {
            if let Some(last) = self.last_redeem_ms.get(condition_id) {
                if now_ms.saturating_sub(*last) < min_interval_ms {
                    return Err(MergeBlockReason::IntervalBackoff);
                }
            }
        }

        let max_ops = self.merge_cfg.max_ops_per_minute;
        if max_ops > 0 && (self.recent_ops.len() as u64) >= max_ops {
            return Err(MergeBlockReason::RateLimit);
        }

        Ok(())
    }

    fn record_redeem(&mut self, condition_id: &str, now_ms: i64) {
        self.last_redeem_ms
            .insert(condition_id.to_string(), now_ms);
        self.recent_ops.push_back(now_ms);
    }

    fn prune_ops(&mut self, now_ms: i64) {
        let cutoff = now_ms.saturating_sub(60_000);
        while let Some(ts) = self.recent_ops.front().copied() {
            if ts < cutoff {
                self.recent_ops.pop_front();
            } else {
                break;
            }
        }
    }

    fn is_fast_move(&self, market: &MarketState, now_ms: i64) -> bool {
        let mut alpha_state = market.alpha.clone();
        let var_per_s = volatility::var_per_s(&alpha_state);
        let market_ws_last_ms = latest_market_ws_ms(market);
        let eval = toxicity::evaluate_regime(
            &mut alpha_state,
            now_ms,
            &self.alpha_cfg,
            &self.oracle_cfg,
            market.rtds_primary,
            market.rtds_sanity,
            market_ws_last_ms,
            var_per_s,
        );
        eval.fast_move
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeBlockReason {
    IntervalBackoff,
    RateLimit,
}

#[derive(Debug, Clone)]
pub struct InventoryExecutor {
    tx_onchain: Sender<OnchainRequest>,
}

impl InventoryExecutor {
    pub fn new(tx_onchain: Sender<OnchainRequest>) -> Self {
        Self { tx_onchain }
    }

    pub async fn execute(&self, actions: &[InventoryAction]) {
        for action in actions {
            match action {
                InventoryAction::Merge {
                    condition_id,
                    qty_sets,
                } => {
                    if self
                        .tx_onchain
                        .try_send(OnchainRequest::Merge {
                            condition_id: condition_id.clone(),
                            qty_sets: *qty_sets,
                        })
                        .is_err()
                    {
                        tracing::warn!(
                            target: "inventory_engine",
                            condition_id = %condition_id,
                            qty_sets,
                            "onchain queue full; dropping merge request"
                        );
                    }
                }
                InventoryAction::Redeem { condition_id } => {
                    if self
                        .tx_onchain
                        .try_send(OnchainRequest::Redeem {
                            condition_id: condition_id.clone(),
                        })
                        .is_err()
                    {
                        tracing::warn!(
                            target: "inventory_engine",
                            condition_id = %condition_id,
                            "onchain queue full; dropping redeem request"
                        );
                    }
                }
                InventoryAction::PauseMerges { .. } => {}
            }
        }
    }
}

#[derive(Debug)]
pub struct InventoryLoop {
    engine: InventoryEngine,
    executor: InventoryExecutor,
    merge_interval_s: u64,
}

impl InventoryLoop {
    pub fn new(engine: InventoryEngine, executor: InventoryExecutor, merge_interval_s: u64) -> Self {
        Self {
            engine,
            executor,
            merge_interval_s,
        }
    }

    pub async fn run(mut self, mut rx_quote: Receiver<QuoteTick>) {
        let interval_s = self.merge_interval_s.max(1);
        let mut tick = time::interval(Duration::from_secs(interval_s));
        let mut latest: HashMap<String, MarketState> = HashMap::new();

        loop {
            tokio::select! {
                maybe_tick = rx_quote.recv() => {
                    match maybe_tick {
                        Some(QuoteTick { slug, state, .. }) => {
                            latest.insert(slug, state);
                        }
                        None => break,
                    }
                }
                _ = tick.tick() => {
                    if latest.is_empty() {
                        continue;
                    }

                    let now_ms = now_ms();
                    let snapshot: Vec<MarketState> = latest.values().cloned().collect();
                    let actions = self.engine.tick(now_ms, &snapshot);

                    if actions.is_empty() {
                        continue;
                    }

                    log_inventory_actions(&actions);

                    self.executor.execute(&actions).await;
                }
            }
        }
    }
}

fn full_sets_u64(up: f64, down: f64) -> u64 {
    let min = up.min(down);
    if !min.is_finite() || min <= 0.0 {
        return 0;
    }
    min.floor() as u64
}

fn latest_market_ws_ms(market: &MarketState) -> Option<i64> {
    let up_ms = market.up_book.last_update_ms;
    let down_ms = market.down_book.last_update_ms;
    match (up_ms > 0, down_ms > 0) {
        (true, true) => Some(up_ms.max(down_ms)),
        (true, false) => Some(up_ms),
        (false, true) => Some(down_ms),
        (false, false) => None,
    }
}

fn log_inventory_actions(actions: &[InventoryAction]) {
    for action in actions {
        match action {
            InventoryAction::Merge {
                condition_id,
                qty_sets,
            } => {
                tracing::info!(
                    target: "inventory_engine",
                    condition_id = %condition_id,
                    qty_sets,
                    "merge decision"
                );
            }
            InventoryAction::Redeem {
                condition_id,
            } => {
                tracing::info!(target: "inventory_engine", condition_id = %condition_id, "redeem decision");
            }
            InventoryAction::PauseMerges { reason } => {
                tracing::info!(
                    target: "inventory_engine",
                    reason = %reason,
                    "merge paused"
                );
            }
        }
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AlphaConfig, MergeConfig, MergeWalletMode, OracleConfig};
    use crate::state::market_state::{MarketIdentity, MarketState};
    use tokio::sync::mpsc;

    fn make_market(condition_id: &str, up: f64, down: f64) -> MarketState {
        let identity = MarketIdentity {
            slug: format!("btc-updown-15m-{condition_id}"),
            interval_start_ts: 0,
            interval_end_ts: 900,
            condition_id: condition_id.to_string(),
            token_up: "UP".to_string(),
            token_down: "DOWN".to_string(),
            active: true,
            closed: false,
            accepting_orders: true,
            restricted: false,
        };
        let mut state = MarketState::new(identity, 0);
        state.inventory.up.shares = up;
        state.inventory.down.shares = down;
        state
    }

    #[test]
    fn merge_threshold_and_batching() {
        let merge_cfg = MergeConfig {
            enabled: true,
            min_sets: 25,
            batch_sets: 25,
            interval_s: 10,
            max_ops_per_minute: 6,
            pause_during_fast_move: false,
            wallet_mode: MergeWalletMode::Eoa,
        };
        let mut engine = InventoryEngine::new(
            merge_cfg,
            AlphaConfig::default(),
            OracleConfig::default(),
        );

        let market_low = make_market("cond-low", 10.0, 30.0);
        let actions = engine.tick(0, &[market_low]);
        assert!(actions.is_empty());

        let market = make_market("cond", 60.0, 55.0);
        let actions = engine.tick(0, &[market]);
        assert_eq!(
            actions,
            vec![InventoryAction::Merge {
                condition_id: "cond".to_string(),
                qty_sets: 50,
            }]
        );
    }

    #[test]
    fn merge_respects_interval_rate_limit() {
        let merge_cfg = MergeConfig {
            enabled: true,
            min_sets: 25,
            batch_sets: 25,
            interval_s: 10,
            max_ops_per_minute: 10,
            pause_during_fast_move: false,
            wallet_mode: MergeWalletMode::Eoa,
        };
        let mut engine = InventoryEngine::new(
            merge_cfg,
            AlphaConfig::default(),
            OracleConfig::default(),
        );

        let market = make_market("cond", 50.0, 50.0);
        let actions = engine.tick(0, &[market.clone()]);
        assert_eq!(actions.len(), 1);

        let actions = engine.tick(5_000, &[market.clone()]);
        assert!(actions.iter().any(|action| {
            matches!(
                action,
                InventoryAction::PauseMerges { reason } if reason == "interval_backoff"
            )
        }));

        let actions = engine.tick(10_000, &[market]);
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn merge_respects_max_ops_per_minute() {
        let merge_cfg = MergeConfig {
            enabled: true,
            min_sets: 25,
            batch_sets: 25,
            interval_s: 1,
            max_ops_per_minute: 1,
            pause_during_fast_move: false,
            wallet_mode: MergeWalletMode::Eoa,
        };
        let mut engine = InventoryEngine::new(
            merge_cfg,
            AlphaConfig::default(),
            OracleConfig::default(),
        );

        let market_a = make_market("cond-a", 30.0, 30.0);
        let market_b = make_market("cond-b", 30.0, 30.0);

        let actions = engine.tick(0, &[market_a, market_b]);
        let merges: Vec<_> = actions
            .into_iter()
            .filter(|action| matches!(action, InventoryAction::Merge { .. }))
            .collect();
        assert_eq!(merges.len(), 1);
    }

    #[test]
    fn wallet_mode_relayer_still_respects_min_sets() {
        let merge_cfg = MergeConfig {
            enabled: true,
            min_sets: 50,
            batch_sets: 25,
            interval_s: 10,
            max_ops_per_minute: 6,
            pause_during_fast_move: false,
            wallet_mode: MergeWalletMode::Relayer,
        };
        let mut engine = InventoryEngine::new(
            merge_cfg,
            AlphaConfig::default(),
            OracleConfig::default(),
        );

        let market = make_market("cond", 30.0, 30.0);
        let actions = engine.tick(0, &[market]);
        assert!(actions.is_empty());

        let market = make_market("cond", 60.0, 55.0);
        let actions = engine.tick(10_000, &[market]);
        assert_eq!(
            actions,
            vec![InventoryAction::Merge {
                condition_id: "cond".to_string(),
                qty_sets: 50,
            }]
        );
    }

    #[tokio::test]
    async fn executor_emits_onchain_requests() {
        let (tx, mut rx) = mpsc::channel(10);
        let exec = InventoryExecutor::new(tx);
        exec.execute(&[
            InventoryAction::Merge {
                condition_id: "cond".to_string(),
                qty_sets: 25,
            },
            InventoryAction::Redeem {
                condition_id: "cond2".to_string(),
            },
        ])
        .await;

        let first = rx.recv().await.expect("first request");
        match first {
            OnchainRequest::Merge { condition_id, qty_sets } => {
                assert_eq!(condition_id, "cond");
                assert_eq!(qty_sets, 25);
            }
            _ => panic!("expected merge request"),
        }
        let second = rx.recv().await.expect("second request");
        match second {
            OnchainRequest::Redeem { condition_id } => {
                assert_eq!(condition_id, "cond2");
            }
            _ => panic!("expected redeem request"),
        }
    }
}
