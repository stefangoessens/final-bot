#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde_json::json;
use tokio::sync::{mpsc, watch};
use tokio::time::{self, MissedTickBehavior};

use crate::clients::clob_rest::ClobRestClient;
use crate::config::{RewardsConfig, TradingConfig};
use crate::persistence::LogEvent;
use crate::state::market_state::MarketState;
use crate::state::state_manager::QuoteTick;
use crate::strategy::DesiredOrder;

#[derive(Debug, Clone, Default)]
pub struct RewardsSnapshot {
    pub now_ms: i64,
    pub scoring: HashMap<String, bool>,
}

pub struct RewardEngine {
    cfg: RewardsConfig,
    rest: ClobRestClient,
    tx_snapshot: watch::Sender<RewardsSnapshot>,
}

impl RewardEngine {
    pub fn new(cfg: RewardsConfig, rest: ClobRestClient) -> (Self, watch::Receiver<RewardsSnapshot>) {
        let (tx, rx) = watch::channel(RewardsSnapshot::default());
        (
            Self {
                cfg,
                rest,
                tx_snapshot: tx,
            },
            rx,
        )
    }

    pub async fn run(
        self,
        mut rx_quote: mpsc::Receiver<QuoteTick>,
        log_tx: Option<mpsc::Sender<LogEvent>>,
    ) {
        if !self.cfg.enable_liquidity_rewards_chasing {
            tracing::info!(
                target: "reward_engine",
                "liquidity rewards chasing disabled; reward engine not started"
            );
            return;
        }

        let interval_s = self.cfg.scoring_check_interval_s.max(1) as u64;
        let mut ticker = time::interval(Duration::from_secs(interval_s));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let mut market_order_ids: HashMap<String, HashSet<String>> = HashMap::new();
        let mut union_order_ids: HashSet<String> = HashSet::new();

        loop {
            tokio::select! {
                maybe_tick = rx_quote.recv() => {
                    match maybe_tick {
                        Some(tick) => {
                            let QuoteTick { slug, state, .. } = tick;
                            let next = extract_level0_order_ids(&state);
                            if let Some(prev) = market_order_ids.insert(slug, next.clone()) {
                                for id in prev {
                                    union_order_ids.remove(&id);
                                }
                            }
                            for id in next {
                                union_order_ids.insert(id);
                            }
                        }
                        None => break,
                    }
                }
                _ = ticker.tick() => {
                    if !self.rest.authenticated() {
                        continue;
                    }
                    if union_order_ids.is_empty() {
                        continue;
                    }

                    let now_ms = now_ms();
                    let ids: Vec<String> = union_order_ids.iter().cloned().collect();
                    match self.rest.are_orders_scoring(&ids).await {
                        Ok(scoring) => {
                            let _ = self.tx_snapshot.send(RewardsSnapshot { now_ms, scoring: scoring.clone() });
                            log_scoring(&log_tx, now_ms, &scoring);
                        }
                        Err(err) => {
                            tracing::warn!(target: "reward_engine", error = %err, "orders scoring check failed");
                            log_scoring_error(&log_tx, now_ms, &err.to_string());
                        }
                    }
                }
            }
        }
    }
}

fn extract_level0_order_ids(state: &MarketState) -> HashSet<String> {
    let mut out = HashSet::new();
    for ((_, level), order) in &state.orders.live {
        if *level == 0 {
            out.insert(order.order_id.clone());
        }
    }
    out
}

pub fn apply_reward_hints(
    desired: &mut [DesiredOrder],
    state: &MarketState,
    trading_cfg: &TradingConfig,
    rewards_cfg: &RewardsConfig,
    rewards: Option<&RewardsSnapshot>,
) -> RewardApplySummary {
    if !rewards_cfg.enable_liquidity_rewards_chasing {
        return RewardApplySummary::default();
    }
    let Some(rewards) = rewards else {
        return RewardApplySummary::default();
    };
    if rewards.scoring.is_empty() {
        return RewardApplySummary::default();
    }

    let target_total = effective_target_total(state, trading_cfg);
    let (cap_up, cap_down) = effective_caps(state, target_total);

    let up_live = state
        .orders
        .live
        .get(&(state.identity.token_up.clone(), 0))
        .and_then(|o| rewards.scoring.get(&o.order_id).copied());
    let down_live = state
        .orders
        .live
        .get(&(state.identity.token_down.clone(), 0))
        .and_then(|o| rewards.scoring.get(&o.order_id).copied());

    let mut summary = RewardApplySummary {
        up_scoring: up_live,
        down_scoring: down_live,
        ..RewardApplySummary::default()
    };

    let Some(up_idx) = desired
        .iter()
        .position(|o| o.token_id == state.identity.token_up && o.level == 0)
    else {
        return summary;
    };
    let Some(down_idx) = desired
        .iter()
        .position(|o| o.token_id == state.identity.token_down && o.level == 0)
    else {
        return summary;
    };

    let mut up_price = desired[up_idx].price;
    let mut down_price = desired[down_idx].price;

    if matches!(up_live, Some(false)) {
        if let Some(p) = try_improve_bid_price(
            up_price,
            state.up_book.tick_size,
            state.up_book.best_ask,
            trading_cfg.min_ticks_from_ask,
            cap_up,
            target_total,
            down_price,
        ) {
            up_price = p;
            summary.up_improved = true;
        }
        if desired[up_idx].size < 2.0 {
            desired[up_idx].size = 2.0;
            summary.up_size_bumped = true;
        }
    }

    if matches!(down_live, Some(false)) {
        if let Some(p) = try_improve_bid_price(
            down_price,
            state.down_book.tick_size,
            state.down_book.best_ask,
            trading_cfg.min_ticks_from_ask,
            cap_down,
            target_total,
            up_price,
        ) {
            down_price = p;
            summary.down_improved = true;
        }
        if desired[down_idx].size < 2.0 {
            desired[down_idx].size = 2.0;
            summary.down_size_bumped = true;
        }
    }

    desired[up_idx].price = up_price;
    desired[down_idx].price = down_price;

    summary
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RewardApplySummary {
    pub up_scoring: Option<bool>,
    pub down_scoring: Option<bool>,
    pub up_improved: bool,
    pub down_improved: bool,
    pub up_size_bumped: bool,
    pub down_size_bumped: bool,
}

fn effective_target_total(state: &MarketState, trading_cfg: &TradingConfig) -> f64 {
    let mut target_total = state.alpha.target_total;
    if !target_total.is_finite() || target_total <= 0.0 {
        target_total = trading_cfg
            .target_total_base
            .clamp(trading_cfg.target_total_min, trading_cfg.target_total_max);
    }
    target_total
}

fn effective_caps(state: &MarketState, target_total: f64) -> (f64, f64) {
    let mut cap_up = state.alpha.cap_up;
    let mut cap_down = state.alpha.cap_down;
    if !cap_up.is_finite() || cap_up <= 0.0 {
        cap_up = 0.5 * target_total;
    }
    if !cap_down.is_finite() || cap_down <= 0.0 {
        cap_down = 0.5 * target_total;
    }
    (cap_up, cap_down)
}

fn try_improve_bid_price(
    current: f64,
    tick: f64,
    best_ask: Option<f64>,
    min_ticks_from_ask: u64,
    cap: f64,
    target_total: f64,
    other_side_price: f64,
) -> Option<f64> {
    if tick <= 0.0 || !current.is_finite() {
        return None;
    }
    let mut candidate = current + tick;
    if let Some(ask) = best_ask {
        let post_only_cap = (ask - min_ticks_from_ask as f64 * tick).max(0.0);
        candidate = candidate.min(post_only_cap);
    }
    candidate = candidate.min(cap).min(0.99);

    if candidate <= current + 1e-12 {
        return None;
    }
    if candidate + other_side_price > target_total + 1e-12 {
        return None;
    }
    Some(floor_to_tick(candidate, tick))
}

fn floor_to_tick(price: f64, tick: f64) -> f64 {
    if tick <= 0.0 || !price.is_finite() {
        return 0.0;
    }
    (price / tick).floor() * tick
}

fn log_scoring(log_tx: &Option<mpsc::Sender<LogEvent>>, ts_ms: i64, scoring: &HashMap<String, bool>) {
    let Some(tx) = log_tx else {
        return;
    };
    let payload = json!({
        "count": scoring.len(),
        "orders": scoring,
    });
    let _ = tx.try_send(LogEvent {
        ts_ms,
        event: "rewards.scoring".to_string(),
        payload,
    });
}

fn log_scoring_error(log_tx: &Option<mpsc::Sender<LogEvent>>, ts_ms: i64, message: &str) {
    let Some(tx) = log_tx else {
        return;
    };
    let payload = json!({ "message": message });
    let _ = tx.try_send(LogEvent {
        ts_ms,
        event: "rewards.scoring_error".to_string(),
        payload,
    });
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
    use crate::state::market_state::{MarketIdentity, MarketState};
    use crate::state::order_state::{LiveOrder, OrderStatus};
    use crate::strategy::TimeInForce;

    fn base_state() -> MarketState {
        let identity = MarketIdentity {
            slug: "btc-updown-15m-0".to_string(),
            interval_start_ts: 0,
            interval_end_ts: 900,
            condition_id: "cond".to_string(),
            token_up: "up".to_string(),
            token_down: "down".to_string(),
            active: true,
            closed: false,
            accepting_orders: true,
            restricted: false,
        };
        let mut state = MarketState::new(identity, 900_000);
        state.alpha.target_total = 0.98;
        state.alpha.cap_up = 0.49;
        state.alpha.cap_down = 0.49;
        state.up_book.tick_size = 0.01;
        state.down_book.tick_size = 0.01;
        state.up_book.best_bid = Some(0.48);
        state.down_book.best_bid = Some(0.48);
        state.up_book.best_ask = Some(0.50);
        state.down_book.best_ask = Some(0.50);

        state.orders.upsert(LiveOrder {
            order_id: "o_up".to_string(),
            token_id: "up".to_string(),
            level: 0,
            price: 0.48,
            size: 1.0,
            remaining: 1.0,
            status: OrderStatus::Open,
            last_update_ms: 0,
        });
        state.orders.upsert(LiveOrder {
            order_id: "o_down".to_string(),
            token_id: "down".to_string(),
            level: 0,
            price: 0.48,
            size: 1.0,
            remaining: 1.0,
            status: OrderStatus::Open,
            last_update_ms: 0,
        });

        state
    }

    fn base_desired() -> Vec<DesiredOrder> {
        vec![
            DesiredOrder {
                token_id: "up".to_string(),
                level: 0,
                price: 0.48,
                size: 1.0,
                post_only: true,
                tif: TimeInForce::Gtc,
            },
            DesiredOrder {
                token_id: "down".to_string(),
                level: 0,
                price: 0.48,
                size: 1.0,
                post_only: true,
                tif: TimeInForce::Gtc,
            },
        ]
    }

    #[test]
    fn applies_price_improvement_when_not_scoring() {
        let state = base_state();
        let mut desired = base_desired();
        let mut snapshot = RewardsSnapshot::default();
        snapshot.scoring.insert("o_up".to_string(), false);
        snapshot.scoring.insert("o_down".to_string(), true);

        let summary = apply_reward_hints(
            &mut desired,
            &state,
            &TradingConfig::default(),
            &RewardsConfig {
                enable_liquidity_rewards_chasing: true,
                require_scoring_orders: false,
                scoring_check_interval_s: 30,
            },
            Some(&snapshot),
        );

        assert_eq!(summary.up_scoring, Some(false));
        assert_eq!(summary.down_scoring, Some(true));
        assert!(summary.up_improved);
        assert_eq!(desired[0].price, 0.49);
        assert!(summary.up_size_bumped);
        assert_eq!(desired[0].size, 2.0);
        assert!(!summary.down_improved);
    }

    #[test]
    fn does_not_violate_combined_cap() {
        let mut state = base_state();
        state.alpha.target_total = 0.96;
        state.alpha.cap_up = 0.49;
        state.alpha.cap_down = 0.49;
        let mut desired = base_desired();
        desired[0].price = 0.48;
        desired[1].price = 0.48;

        let mut snapshot = RewardsSnapshot::default();
        snapshot.scoring.insert("o_up".to_string(), false);
        snapshot.scoring.insert("o_down".to_string(), true);

        let summary = apply_reward_hints(
            &mut desired,
            &state,
            &TradingConfig::default(),
            &RewardsConfig {
                enable_liquidity_rewards_chasing: true,
                require_scoring_orders: false,
                scoring_check_interval_s: 30,
            },
            Some(&snapshot),
        );

        assert_eq!(summary.up_improved, false);
        assert_eq!(desired[0].price, 0.48);
    }
}
