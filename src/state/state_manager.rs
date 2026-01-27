use std::collections::HashMap;

use tokio::sync::mpsc::{Receiver, Sender};

use crate::config::{AlphaConfig, OracleConfig, TradingConfig};
use crate::ops::health::HealthState;
use crate::ops::metrics::Metrics;
use crate::state::inventory::TokenSide;
use crate::state::market_state::{MarketIdentity, MarketState};

#[derive(Debug, Clone)]
#[allow(dead_code)] // fields will be used by StrategyEngine + OrderManager in later tasks
pub struct QuoteTick {
    pub slug: String,
    pub now_ms: i64,
    pub state: MarketState,
}

#[derive(Debug, Clone)]
pub struct MarketWsUpdate {
    pub token_id: String,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub tick_size: Option<f64>,
    pub ts_ms: i64,
}

#[derive(Debug, Clone)]
pub struct FillEvent {
    pub token_id: String,
    pub price: f64,
    pub shares: f64,
    pub ts_ms: i64,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // populated by the user WS client task
pub enum UserWsUpdate {
    Fill(FillEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // variants are used once RTDS client lands
pub enum RTDSSource {
    BinanceBtcUsdt,
    ChainlinkBtcUsd,
}

#[derive(Debug, Clone)]
pub struct RTDSUpdate {
    pub source: RTDSSource,
    pub price: f64,
    pub ts_ms: i64,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // event variants will be used by feed + strategy tasks
pub enum AppEvent {
    MarketDiscovered(MarketIdentity),
    SetTrackedMarkets { slugs: Vec<String> },
    MarketWsUpdate(MarketWsUpdate),
    UserWsUpdate(UserWsUpdate),
    RTDSUpdate(RTDSUpdate),
    TimerTick { now_ms: i64 },
}

pub struct StateManager {
    markets: HashMap<String, MarketState>, // key: slug
    last_user_ws_msg_ms: Option<i64>,
    alpha_cfg: AlphaConfig,
    oracle_cfg: OracleConfig,
    trading_cfg: TradingConfig,
    health: HealthState,
    metrics: Metrics,
}

impl StateManager {
    pub fn new(
        alpha_cfg: AlphaConfig,
        oracle_cfg: OracleConfig,
        trading_cfg: TradingConfig,
        health: HealthState,
        metrics: Metrics,
    ) -> Self {
        Self {
            markets: HashMap::new(),
            last_user_ws_msg_ms: None,
            alpha_cfg,
            oracle_cfg,
            trading_cfg,
            health,
            metrics,
        }
    }

    #[cfg(test)]
    pub fn market_state(&self, slug: &str) -> Option<&MarketState> {
        self.markets.get(slug)
    }

    pub async fn run(mut self, mut rx: Receiver<AppEvent>, tx_quote: Sender<QuoteTick>) {
        while let Some(event) = rx.recv().await {
            let (now_ms, is_timer) = match &event {
                AppEvent::TimerTick { now_ms } => (*now_ms, true),
                AppEvent::MarketWsUpdate(u) => (u.ts_ms, false),
                AppEvent::UserWsUpdate(UserWsUpdate::Fill(f)) => (f.ts_ms, false),
                AppEvent::RTDSUpdate(u) => (u.ts_ms, false),
                AppEvent::MarketDiscovered(_) | AppEvent::SetTrackedMarkets { .. } => {
                    (now_ms(), false)
                }
            };

            self.apply_event(event, now_ms);

            if is_timer {
                for (slug, state) in &self.markets {
                    let _ = tx_quote
                        .send(QuoteTick {
                            slug: slug.clone(),
                            now_ms,
                            state: state.clone(),
                        })
                        .await;
                }
            }
        }
    }

    fn apply_event(&mut self, event: AppEvent, now_ms: i64) {
        match event {
            AppEvent::MarketDiscovered(identity) => {
                let slug = identity.slug.clone();
                let cutoff_ts_ms = identity
                    .interval_end_ts
                    .saturating_mul(1_000)
                    .saturating_sub(60_000);

                match self.markets.get_mut(&slug) {
                    Some(existing) => {
                        existing.identity = identity;
                        existing.cutoff_ts_ms = cutoff_ts_ms;
                    }
                    None => {
                        self.markets
                            .insert(slug, MarketState::new(identity, cutoff_ts_ms));
                    }
                }
            }
            AppEvent::SetTrackedMarkets { slugs } => {
                self.health.set_tracked_markets(slugs.len());
                let keep: std::collections::HashSet<String> = slugs.into_iter().collect();
                self.markets.retain(|slug, _| keep.contains(slug));
            }
            AppEvent::MarketWsUpdate(update) => {
                self.health.mark_market_ws(update.ts_ms);
                self.apply_market_ws_update(update);
            }
            AppEvent::UserWsUpdate(update) => {
                self.health.mark_user_ws(now_ms);
                self.apply_user_ws_update(update, now_ms);
            }
            AppEvent::RTDSUpdate(update) => {
                match update.source {
                    RTDSSource::BinanceBtcUsdt => self.health.mark_binance(update.ts_ms),
                    RTDSSource::ChainlinkBtcUsd => self.health.mark_chainlink(update.ts_ms),
                }
                self.apply_rtds_update(update);
            }
            AppEvent::TimerTick { .. } => {
                self.update_health_metrics(now_ms);
                for state in self.markets.values_mut() {
                    let out = crate::alpha::update_alpha(
                        state,
                        now_ms,
                        &self.alpha_cfg,
                        &self.oracle_cfg,
                        &self.trading_cfg,
                    );
                    state.quoting_enabled = now_ms < state.cutoff_ts_ms && out.size_scalar > 0.0;
                    tracing::debug!(
                        target: "alpha",
                        slug = %state.identity.slug,
                        regime = ?out.regime,
                        oracle_disagree = out.oracle_disagree,
                        q_up = out.q_up,
                        cap_up = out.cap_up,
                        cap_down = out.cap_down,
                        target_total = out.target_total,
                        size_scalar = out.size_scalar,
                        "alpha update"
                    );
                }
            }
        }
    }

    fn apply_market_ws_update(&mut self, update: MarketWsUpdate) {
        for state in self.markets.values_mut() {
            if update.token_id == state.identity.token_up {
                state.up_book.best_bid = update.best_bid;
                state.up_book.best_ask = update.best_ask;
                if let Some(tick) = update.tick_size {
                    state.up_book.tick_size = tick;
                }
                state.up_book.last_update_ms = update.ts_ms;
                return;
            }
            if update.token_id == state.identity.token_down {
                state.down_book.best_bid = update.best_bid;
                state.down_book.best_ask = update.best_ask;
                if let Some(tick) = update.tick_size {
                    state.down_book.tick_size = tick;
                }
                state.down_book.last_update_ms = update.ts_ms;
                return;
            }
        }
    }

    fn apply_user_ws_update(&mut self, update: UserWsUpdate, now_ms: i64) {
        self.last_user_ws_msg_ms = Some(now_ms);
        match update {
            UserWsUpdate::Fill(fill) => self.apply_fill(fill),
        }
    }

    fn apply_fill(&mut self, fill: FillEvent) {
        for state in self.markets.values_mut() {
            let side: TokenSide = match state.token_side(&fill.token_id) {
                Some(s) => s,
                None => continue,
            };
            state
                .inventory
                .apply_buy_fill(side, fill.price, fill.shares, fill.ts_ms);
            self.metrics.inc_fills();
            return;
        }
    }

    fn apply_rtds_update(&mut self, update: RTDSUpdate) {
        for state in self.markets.values_mut() {
            match update.source {
                RTDSSource::BinanceBtcUsdt => {
                    state.rtds_primary = Some(crate::state::rtds_price::RTDSPrice {
                        price: update.price,
                        ts_ms: update.ts_ms,
                    });
                }
                RTDSSource::ChainlinkBtcUsd => {
                    state.rtds_sanity = Some(crate::state::rtds_price::RTDSPrice {
                        price: update.price,
                        ts_ms: update.ts_ms,
                    });
                }
            }
        }
    }

    fn update_health_metrics(&self, now_ms: i64) {
        let report = self.health.report(now_ms);
        self.metrics
            .set_ws_market_connected(report.feeds.market_ws.status == "healthy");
        self.metrics
            .set_ws_user_connected(report.feeds.user_ws.status == "healthy");
        let rtds_connected = report.feeds.rtds_chainlink.status == "healthy"
            && report.feeds.rtds_binance.status == "healthy";
        self.metrics.set_rtds_connected(rtds_connected);
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
    use crate::config::{AlphaConfig, AppConfig, OracleConfig, TradingConfig};
    use crate::ops::OpsState;

    fn make_identity() -> MarketIdentity {
        MarketIdentity {
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
        }
    }

    #[test]
    fn applying_fill_updates_cost_basis() {
        let ops = OpsState::new(&AppConfig::default());
        let mut sm = StateManager::new(
            AlphaConfig::default(),
            OracleConfig::default(),
            TradingConfig::default(),
            ops.health,
            ops.metrics,
        );
        sm.apply_event(AppEvent::MarketDiscovered(make_identity()), 0);

        sm.apply_event(
            AppEvent::UserWsUpdate(UserWsUpdate::Fill(FillEvent {
                token_id: "up".to_string(),
                price: 0.49,
                shares: 10.0,
                ts_ms: 1_000,
            })),
            1_000,
        );

        let state = sm.market_state("btc-updown-15m-0").unwrap();
        assert_eq!(state.inventory.up.shares, 10.0);
        assert_eq!(state.inventory.up.notional_usdc, 4.9);
        let avg = state.inventory.up.avg_cost().unwrap();
        assert!((avg - 0.49).abs() < 1e-12, "avg_cost={avg}");
        assert_eq!(state.inventory.unpaired_shares(), 10.0);
    }

    #[test]
    fn market_ws_update_sets_staleness_timestamp() {
        let ops = OpsState::new(&AppConfig::default());
        let mut sm = StateManager::new(
            AlphaConfig::default(),
            OracleConfig::default(),
            TradingConfig::default(),
            ops.health,
            ops.metrics,
        );
        sm.apply_event(AppEvent::MarketDiscovered(make_identity()), 0);

        sm.apply_event(
            AppEvent::MarketWsUpdate(MarketWsUpdate {
                token_id: "down".to_string(),
                best_bid: Some(0.49),
                best_ask: Some(0.51),
                tick_size: Some(0.001),
                ts_ms: 2_000,
            }),
            2_000,
        );

        let state = sm.market_state("btc-updown-15m-0").unwrap();
        assert_eq!(state.down_book.last_update_ms, 2_000);
        assert_eq!(state.down_book.tick_size, 0.001);
        assert_eq!(state.down_book.best_bid, Some(0.49));
        assert_eq!(state.down_book.best_ask, Some(0.51));
    }
}
