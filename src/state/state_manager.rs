use std::collections::HashMap;

use tokio::sync::mpsc::{error::TrySendError, Receiver, Sender};

use crate::config::{AlphaConfig, OracleConfig, TradingConfig};
use crate::ops::health::HealthState;
use crate::ops::metrics::Metrics;
use crate::state::inventory::{InventorySide, TokenSide, USDC_BASE_UNITS_F64};
use crate::state::market_state::{MarketIdentity, MarketState};
use crate::state::order_state::LiveOrder;

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
pub struct TickSizeSeed {
    pub token_id: String,
    pub tick_size: f64,
    pub ts_ms: i64,
}

#[derive(Debug, Clone)]
pub struct FillEvent {
    pub token_id: String,
    pub side: OrderSide,
    pub price: f64,
    pub shares: f64,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // populated by the user WS client task
pub enum UserWsUpdate {
    Fill(FillEvent),
    Heartbeat { ts_ms: i64 },
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
pub struct GeoblockStatus {
    pub blocked: Option<bool>,
    pub ip: Option<String>,
    pub country: Option<String>,
    pub region: Option<String>,
    pub ts_ms: i64,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum OrderUpdate {
    Upsert {
        slug: String,
        order: LiveOrder,
    },
    Remove {
        slug: String,
        token_id: String,
        side: OrderSide,
        level: usize,
        order_id: String,
        ts_ms: i64,
    },
}

impl OrderUpdate {
    fn ts_ms(&self) -> i64 {
        match self {
            OrderUpdate::Upsert { order, .. } => order.last_update_ms,
            OrderUpdate::Remove { ts_ms, .. } => *ts_ms,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OrderSeed {
    pub slug: String,
    pub orders: Vec<LiveOrder>,
    pub ts_ms: i64,
}

#[derive(Debug, Clone)]
pub struct InventorySeed {
    pub slug: String,
    pub up: InventorySide,
    pub down: InventorySide,
    pub ts_ms: i64,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // event variants will be used by feed + strategy tasks
pub enum AppEvent {
    MarketDiscovered(MarketIdentity),
    SetTrackedMarkets {
        slugs: Vec<String>,
    },
    MarketWsUpdate(MarketWsUpdate),
    TickSizeSeed(TickSizeSeed),
    UserWsUpdate(UserWsUpdate),
    RTDSUpdate(RTDSUpdate),
    GeoblockStatus(GeoblockStatus),
    SeedOrders(OrderSeed),
    SeedInventory(InventorySeed),
    // W7.15: OrderManager emits live order updates for rewards scoring.
    OrderUpdate(OrderUpdate),
    OnchainMerge {
        condition_id: String,
        qty_base_units: u64,
        ts_ms: i64,
    },
    OnchainRedeem {
        condition_id: String,
        ts_ms: i64,
    },
    TimerTick {
        now_ms: i64,
    },
}

pub struct StateManager {
    markets: HashMap<String, MarketState>,           // key: slug
    last_fill_quote_tick_ms: HashMap<String, i64>,   // key: slug
    last_market_quote_tick_ms: HashMap<String, i64>, // key: slug
    last_user_ws_msg_ms: Option<i64>,
    last_geoblock: Option<GeoblockStatus>,
    markout: HashMap<String, FillMarkoutTracker>, // key: slug
    alpha_cfg: AlphaConfig,
    oracle_cfg: OracleConfig,
    trading_cfg: TradingConfig,
    health: HealthState,
    metrics: Metrics,
}

#[derive(Debug, Clone)]
struct MarkoutPendingFill {
    token_side: TokenSide,
    fill_side: OrderSide,
    fill_ts_ms: i64,
    btc_fill_price: f64,
    done_short: bool,
    done_long: bool,
}

#[derive(Debug, Default, Clone)]
struct FillMarkoutTracker {
    pending: Vec<MarkoutPendingFill>,
    ewma_bps: f64,
    fills_seen: u64,
}

impl FillMarkoutTracker {
    fn record_fill(
        &mut self,
        token_side: TokenSide,
        fill_side: OrderSide,
        fill_ts_ms: i64,
        btc_fill_price: f64,
    ) {
        if fill_ts_ms <= 0 || !btc_fill_price.is_finite() || btc_fill_price <= 0.0 {
            return;
        }

        self.pending.push(MarkoutPendingFill {
            token_side,
            fill_side,
            fill_ts_ms,
            btc_fill_price,
            done_short: false,
            done_long: false,
        });
    }

    fn evaluate(
        &mut self,
        alpha: &mut crate::state::market_state::AlphaState,
        now_ms: i64,
        btc_now: f64,
        cfg: &TradingConfig,
    ) {
        if !cfg.markout_cooldown_enabled {
            return;
        }
        if now_ms <= 0 || !btc_now.is_finite() || btc_now <= 0.0 {
            return;
        }

        let short_ms = cfg.markout_horizon_short_ms.max(1);
        let long_ms = cfg.markout_horizon_long_ms.max(short_ms + 1);
        let threshold_bps = cfg.markout_bad_threshold_bps.max(0.0);
        let cooldown_ms = cfg.markout_cooldown_ms.max(1);

        let a = cfg.markout_ewma_alpha.clamp(0.0, 1.0);
        let fills_seen = &mut self.fills_seen;
        let ewma_bps = &mut self.ewma_bps;
        let mut apply_ewma = |m: f64| {
            if !m.is_finite() {
                return;
            }
            *fills_seen = fills_seen.saturating_add(1);
            if *fills_seen == 1 || a >= 1.0 - 1e-12 {
                *ewma_bps = m;
            } else if a > 0.0 {
                *ewma_bps = a * m + (1.0 - a) * *ewma_bps;
            }
            alpha.markout_ewma_bps = *ewma_bps;
        };

        let mut any_bad = false;
        let mut any_bad_hard = false;
        for pending in self.pending.iter_mut() {
            if !pending.done_short && now_ms.saturating_sub(pending.fill_ts_ms) >= short_ms {
                let m = markout_bps(
                    pending.btc_fill_price,
                    btc_now,
                    pending.token_side,
                    pending.fill_side,
                );
                alpha.last_markout_bps_short = Some(m);
                apply_ewma(m);
                pending.done_short = true;
                if m < -threshold_bps {
                    any_bad = true;
                }
                if threshold_bps > 0.0 && m < -2.0 * threshold_bps {
                    any_bad_hard = true;
                }
            }

            if !pending.done_long && now_ms.saturating_sub(pending.fill_ts_ms) >= long_ms {
                let m = markout_bps(
                    pending.btc_fill_price,
                    btc_now,
                    pending.token_side,
                    pending.fill_side,
                );
                alpha.last_markout_bps_long = Some(m);
                apply_ewma(m);
                pending.done_long = true;
                if m < -threshold_bps {
                    any_bad = true;
                }
                if threshold_bps > 0.0 && m < -2.0 * threshold_bps {
                    any_bad_hard = true;
                }
            }
        }

        // Prune fills once all horizons are evaluated.
        self.pending.retain(|p| !(p.done_short && p.done_long));

        if any_bad_hard || (any_bad && (self.fills_seen >= cfg.markout_min_fills_before_activation))
        {
            alpha.maker_cooldown_until_ms = alpha
                .maker_cooldown_until_ms
                .max(now_ms.saturating_add(cooldown_ms));
        }
    }
}

fn markout_bps(btc_fill: f64, btc_now: f64, token_side: TokenSide, fill_side: OrderSide) -> f64 {
    if !btc_fill.is_finite() || btc_fill <= 0.0 || !btc_now.is_finite() || btc_now <= 0.0 {
        return 0.0;
    }
    let r_bps = 10_000.0 * (btc_now / btc_fill - 1.0);
    let token_dir = match token_side {
        TokenSide::Up => 1.0,
        TokenSide::Down => -1.0,
    };
    let side_dir = match fill_side {
        OrderSide::Buy => 1.0,
        OrderSide::Sell => -1.0,
    };
    token_dir * side_dir * r_bps
}

fn update_markout_for_market(
    markout: &mut HashMap<String, FillMarkoutTracker>,
    state: &mut MarketState,
    now_ms: i64,
    cfg: &TradingConfig,
) {
    if !cfg.markout_cooldown_enabled {
        return;
    }
    // If Binance is stale, the reference price can be untrustworthy; skip markout evaluation.
    if state.alpha.binance_stale {
        return;
    }

    let Some(btc) = state.rtds_primary.as_ref() else {
        return;
    };
    if !btc.price.is_finite() || btc.price <= 0.0 {
        return;
    }

    markout
        .entry(state.identity.slug.clone())
        .or_default()
        .evaluate(&mut state.alpha, now_ms, btc.price, cfg);
}

fn update_pair_protection_for_market(state: &mut MarketState, now_ms: i64, cfg: &TradingConfig) {
    let up = state.inventory.up.shares.max(0.0);
    let down = state.inventory.down.shares.max(0.0);
    let max = up.max(down);
    let min = up.min(down);
    state.alpha.pair_ratio = if max > 0.0 { min / max } else { 1.0 };

    let unpaired = (up - down).abs();
    let is_unpaired = unpaired > 1e-9;
    let duration_s = if is_unpaired {
        let since = state.alpha.unpaired_since_ms.unwrap_or(now_ms);
        (now_ms.saturating_sub(since) as f64 / 1_000.0).max(0.0)
    } else {
        0.0
    };
    state.alpha.unpaired_duration_s = duration_s;

    if !is_unpaired || !cfg.pair_protection_enabled {
        state.alpha.pair_protection_level = 0.0;
        return;
    }

    let total_shares = up + down;
    if total_shares + 1e-12 < cfg.pair_protection_min_total_shares.max(0.0) {
        state.alpha.pair_protection_level = 0.0;
        return;
    }

    let ratio_threshold = cfg.pair_protection_ratio_threshold;
    let level_ratio = if state.alpha.pair_ratio + 1e-12 < ratio_threshold {
        ((ratio_threshold - state.alpha.pair_ratio) / ratio_threshold).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let dur_threshold_s = cfg.pair_protection_unpaired_duration_s.max(0) as f64;
    let level_dur = if dur_threshold_s > 0.0 && duration_s + 1e-12 >= dur_threshold_s {
        ((duration_s - dur_threshold_s) / dur_threshold_s).clamp(0.0, 1.0)
    } else {
        0.0
    };

    state.alpha.pair_protection_level = level_ratio.max(level_dur);
}

impl StateManager {
    const FILL_QUOTE_TICK_MIN_GAP_MS: i64 = 25;
    const MARKET_QUOTE_TICK_MIN_GAP_MS: i64 = 25;

    pub fn new(
        alpha_cfg: AlphaConfig,
        oracle_cfg: OracleConfig,
        trading_cfg: TradingConfig,
        health: HealthState,
        metrics: Metrics,
    ) -> Self {
        Self {
            markets: HashMap::new(),
            last_fill_quote_tick_ms: HashMap::new(),
            last_market_quote_tick_ms: HashMap::new(),
            last_user_ws_msg_ms: None,
            last_geoblock: None,
            markout: HashMap::new(),
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
            let market_update_slug = match &event {
                AppEvent::MarketWsUpdate(u) => self.market_slug_for_token_id(&u.token_id),
                _ => None,
            };

            let market_update_before = market_update_slug
                .as_deref()
                .and_then(|slug| self.markets.get(slug))
                .map(book_snapshot);
            let market_update_under_floor_before = market_update_slug
                .as_deref()
                .and_then(|slug| self.markets.get(slug))
                .map(|state| market_under_floor(state, self.trading_cfg.min_quote_price))
                .unwrap_or(false);

            let fill_token_id = match &event {
                AppEvent::UserWsUpdate(UserWsUpdate::Fill(fill)) => Some(fill.token_id.clone()),
                _ => None,
            };

            let (now_ms, is_timer) = match &event {
                AppEvent::TimerTick { now_ms } => (*now_ms, true),
                AppEvent::MarketWsUpdate(u) => (u.ts_ms, false),
                AppEvent::TickSizeSeed(u) => (u.ts_ms, false),
                AppEvent::UserWsUpdate(UserWsUpdate::Fill(f)) => (f.ts_ms, false),
                AppEvent::UserWsUpdate(UserWsUpdate::Heartbeat { ts_ms }) => (*ts_ms, false),
                AppEvent::RTDSUpdate(u) => (u.ts_ms, false),
                AppEvent::GeoblockStatus(u) => (u.ts_ms, false),
                AppEvent::SeedOrders(seed) => (seed.ts_ms, false),
                AppEvent::SeedInventory(seed) => (seed.ts_ms, false),
                AppEvent::OrderUpdate(u) => (u.ts_ms(), false),
                AppEvent::OnchainMerge { ts_ms, .. } => (*ts_ms, false),
                AppEvent::OnchainRedeem { ts_ms, .. } => (*ts_ms, false),
                AppEvent::MarketDiscovered(_) | AppEvent::SetTrackedMarkets { .. } => {
                    (now_ms(), false)
                }
            };

            self.apply_event(event, now_ms);

            if is_timer {
                for (slug, state) in &self.markets {
                    let tick = QuoteTick {
                        slug: slug.clone(),
                        now_ms,
                        state: state.clone(),
                    };
                    if let Err(err) = tx_quote.try_send(tick) {
                        match err {
                            TrySendError::Full(_) => {
                                tracing::debug!(
                                    target: "state_manager",
                                    slug = %slug,
                                    "quote tick channel full; dropping tick"
                                );
                            }
                            TrySendError::Closed(_) => {
                                tracing::warn!(
                                    target: "state_manager",
                                    "quote tick channel closed; dropping tick"
                                );
                            }
                        }
                    }
                }
            } else if let Some(slug) = market_update_slug {
                if let Some(state) = self.markets.get(&slug) {
                    let after = book_snapshot(state);
                    let under_floor_after =
                        market_under_floor(state, self.trading_cfg.min_quote_price);
                    let changed = market_update_before
                        .map(|before| before != after)
                        .unwrap_or(true)
                        || market_update_under_floor_before != under_floor_after;

                    if changed {
                        self.maybe_emit_market_quote_tick(slug, now_ms, &tx_quote);
                    }
                }
            } else if let Some(token_id) = fill_token_id {
                if let Some(slug) = self.market_slug_for_token_id(&token_id) {
                    self.maybe_emit_fill_quote_tick(slug, now_ms, &tx_quote);
                }
            }
        }
    }

    fn maybe_emit_market_quote_tick(
        &mut self,
        slug: String,
        now_ms: i64,
        tx_quote: &Sender<QuoteTick>,
    ) {
        if let Some(last_ms) = self.last_market_quote_tick_ms.get(&slug).copied() {
            if now_ms.saturating_sub(last_ms) < Self::MARKET_QUOTE_TICK_MIN_GAP_MS {
                return;
            }
        }

        let Some(state) = self.markets.get(&slug) else {
            return;
        };

        let tick = QuoteTick {
            slug: slug.clone(),
            now_ms,
            state: state.clone(),
        };

        match tx_quote.try_send(tick) {
            Ok(()) => {
                self.last_market_quote_tick_ms.insert(slug, now_ms);
            }
            Err(TrySendError::Full(_)) => {
                tracing::debug!(
                    target: "state_manager",
                    slug = %slug,
                    "quote tick channel full; dropping market-triggered tick"
                );
            }
            Err(TrySendError::Closed(_)) => {
                tracing::warn!(
                    target: "state_manager",
                    "quote tick channel closed; dropping market-triggered tick"
                );
            }
        }
    }

    fn maybe_emit_fill_quote_tick(
        &mut self,
        slug: String,
        now_ms: i64,
        tx_quote: &Sender<QuoteTick>,
    ) {
        if let Some(last_ms) = self.last_fill_quote_tick_ms.get(&slug).copied() {
            if now_ms.saturating_sub(last_ms) < Self::FILL_QUOTE_TICK_MIN_GAP_MS {
                tracing::debug!(
                    target: "state_manager",
                    slug = %slug,
                    now_ms,
                    last_ms,
                    min_gap_ms = Self::FILL_QUOTE_TICK_MIN_GAP_MS,
                    "fill-triggered quote tick throttled"
                );
                return;
            }
        }

        let Some(state) = self.markets.get(&slug) else {
            return;
        };

        let tick = QuoteTick {
            slug: slug.clone(),
            now_ms,
            state: state.clone(),
        };

        match tx_quote.try_send(tick) {
            Ok(()) => {
                self.last_fill_quote_tick_ms.insert(slug, now_ms);
            }
            Err(TrySendError::Full(_)) => {
                tracing::debug!(
                    target: "state_manager",
                    slug = %slug,
                    "quote tick channel full; dropping fill-triggered tick"
                );
            }
            Err(TrySendError::Closed(_)) => {
                tracing::warn!(
                    target: "state_manager",
                    "quote tick channel closed; dropping fill-triggered tick"
                );
            }
        }
    }

    fn market_slug_for_token_id(&self, token_id: &str) -> Option<String> {
        for (slug, state) in &self.markets {
            if state.token_side(token_id).is_some() {
                return Some(slug.clone());
            }
        }
        None
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
                self.last_fill_quote_tick_ms
                    .retain(|slug, _| keep.contains(slug));
                self.last_market_quote_tick_ms
                    .retain(|slug, _| keep.contains(slug));
                self.markout.retain(|slug, _| keep.contains(slug));
            }
            AppEvent::MarketWsUpdate(update) => {
                self.health.mark_market_ws(update.ts_ms);
                self.apply_market_ws_update(update);
            }
            AppEvent::TickSizeSeed(seed) => {
                self.apply_tick_size_seed(seed);
            }
            AppEvent::UserWsUpdate(update) => {
                if let UserWsUpdate::Heartbeat { ts_ms } = &update {
                    self.health.mark_user_ws(*ts_ms);
                }
                self.apply_user_ws_update(update, now_ms);
            }
            AppEvent::RTDSUpdate(update) => {
                match update.source {
                    RTDSSource::BinanceBtcUsdt => self.health.mark_binance(update.ts_ms),
                    RTDSSource::ChainlinkBtcUsd => self.health.mark_chainlink(update.ts_ms),
                }
                self.apply_rtds_update(update);
            }
            AppEvent::GeoblockStatus(status) => {
                self.apply_geoblock_status(status);
            }
            AppEvent::SeedOrders(seed) => {
                self.apply_order_seed(seed);
            }
            AppEvent::SeedInventory(seed) => {
                self.apply_inventory_seed(seed);
            }
            AppEvent::OrderUpdate(update) => {
                self.apply_order_update(update);
            }
            AppEvent::OnchainMerge {
                condition_id,
                qty_base_units,
                ..
            } => {
                self.apply_onchain_merge(&condition_id, qty_base_units);
            }
            AppEvent::OnchainRedeem { condition_id, .. } => {
                self.apply_onchain_redeem(&condition_id);
            }
            AppEvent::TimerTick { .. } => {
                self.update_health_metrics(now_ms);
                let user_ws_fresh = if self.trading_cfg.dry_run {
                    true
                } else {
                    self.health.user_ws_fresh(now_ms)
                };
                let mut enabled_markets = 0usize;
                let mut total_markets = 0usize;
                let mut first_block_reason: Option<String> = None;

                {
                    let alpha_cfg = &self.alpha_cfg;
                    let oracle_cfg = &self.oracle_cfg;
                    let trading_cfg = &self.trading_cfg;
                    let metrics = &self.metrics;
                    let health = &self.health;
                    let markout = &mut self.markout;

                    for state in self.markets.values_mut() {
                        total_markets += 1;

                        let out = crate::alpha::update_alpha(
                            state,
                            now_ms,
                            alpha_cfg,
                            oracle_cfg,
                            trading_cfg,
                        );

                        update_pair_protection_for_market(state, now_ms, trading_cfg);
                        update_markout_for_market(markout, state, now_ms, trading_cfg);

                        let (inventory_usdc, open_order_usdc, exposure_usdc) =
                            market_exposure_usdc(state);
                        let exposure_cap = trading_cfg.max_usdc_exposure_per_market;
                        let exposure_over_cap = exposure_usdc > exposure_cap;
                        metrics.set_market_exposure_usdc(&state.identity.slug, exposure_usdc);
                        metrics.set_market_exposure_cap_usdc(&state.identity.slug, exposure_cap);
                        metrics
                            .set_market_exposure_over_cap(&state.identity.slug, exposure_over_cap);

                        let tradable = state.identity.active
                            && state.identity.accepting_orders
                            && !state.identity.closed;

                        let halted = health.is_halted();
                        let mut block_reason = None;
                        if halted {
                            block_reason =
                                health.halt_reason().or_else(|| Some("halted".to_string()));
                        } else if !user_ws_fresh {
                            block_reason = Some("user_ws_stale".to_string());
                        } else if !tradable {
                            block_reason =
                                Some(format!("market_not_tradable: {}", state.identity.slug));
                        } else if exposure_over_cap {
                            block_reason = Some("exposure_cap".to_string());
                        }

                        let alpha_ok = now_ms < state.cutoff_ts_ms && out.size_scalar > 0.0;
                        let quoting_enabled = alpha_ok && block_reason.is_none();
                        state.quoting_enabled = quoting_enabled;

                        if quoting_enabled {
                            enabled_markets += 1;
                        } else if first_block_reason.is_none() {
                            first_block_reason = block_reason;
                        }

                        if exposure_over_cap {
                            tracing::warn!(
                                target: "risk",
                                slug = %state.identity.slug,
                                exposure_usdc,
                                exposure_cap,
                                inventory_usdc,
                                open_order_usdc,
                                "market exposure over cap; quoting disabled"
                            );
                        }

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
                            quoting_enabled,
                            "alpha update"
                        );
                    }
                }

                let block_reason = if enabled_markets == 0 {
                    if let Some(reason) = first_block_reason {
                        Some(reason)
                    } else if total_markets == 0 {
                        Some("no_markets_tracked".to_string())
                    } else {
                        Some("quoting_disabled".to_string())
                    }
                } else {
                    None
                };
                self.health
                    .set_quoting_status(enabled_markets, block_reason);
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

    fn apply_tick_size_seed(&mut self, seed: TickSizeSeed) {
        if !seed.tick_size.is_finite() || seed.tick_size <= 0.0 {
            tracing::warn!(
                target: "state_manager",
                token_id = %seed.token_id,
                tick_size = seed.tick_size,
                "invalid tick size seed"
            );
            return;
        }

        for state in self.markets.values_mut() {
            if seed.token_id == state.identity.token_up {
                if state.up_book.tick_size <= 0.0 {
                    state.up_book.tick_size = seed.tick_size;
                }
                return;
            }
            if seed.token_id == state.identity.token_down {
                if state.down_book.tick_size <= 0.0 {
                    state.down_book.tick_size = seed.tick_size;
                }
                return;
            }
        }
    }

    fn apply_user_ws_update(&mut self, update: UserWsUpdate, now_ms: i64) {
        self.last_user_ws_msg_ms = Some(now_ms);
        match update {
            UserWsUpdate::Fill(fill) => self.apply_fill(fill),
            UserWsUpdate::Heartbeat { .. } => {}
        }
    }

    fn apply_fill(&mut self, fill: FillEvent) {
        for state in self.markets.values_mut() {
            let side: TokenSide = match state.token_side(&fill.token_id) {
                Some(s) => s,
                None => continue,
            };
            let was_unpaired = state.inventory.unpaired_shares() > 1e-9;
            match fill.side {
                OrderSide::Buy => {
                    state
                        .inventory
                        .apply_buy_fill(side, fill.price, fill.shares, fill.ts_ms)
                }
                OrderSide::Sell => state
                    .inventory
                    .apply_sell_fill(side, fill.shares, fill.ts_ms),
            }

            // Track unpaired start time for pair-protection heuristics.
            let now_unpaired = state.inventory.unpaired_shares() > 1e-9;
            if now_unpaired && !was_unpaired {
                state.alpha.unpaired_since_ms = Some(fill.ts_ms);
            }
            if !now_unpaired {
                state.alpha.unpaired_since_ms = None;
                state.alpha.unpaired_duration_s = 0.0;
                state.alpha.pair_protection_level = 0.0;
            }

            // Record the BTC price at fill time for markout computation.
            if self.trading_cfg.markout_cooldown_enabled {
                if let Some(btc) = state.rtds_primary.as_ref() {
                    if btc.price.is_finite() && btc.price > 0.0 {
                        self.markout
                            .entry(state.identity.slug.clone())
                            .or_default()
                            .record_fill(side, fill.side, fill.ts_ms, btc.price);
                    }
                }
            }

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

    fn apply_geoblock_status(&mut self, status: GeoblockStatus) {
        self.last_geoblock = Some(status.clone());

        // Geoblock only gates trading. In dry-run mode we still capture status for observability,
        // but avoid marking the process halted.
        if self.trading_cfg.dry_run {
            return;
        }

        match status.blocked {
            Some(true) => {
                self.health.set_halt_reason(Some(format!(
                    "geoblock_blocked country={} region={} ip={}",
                    status.country.as_deref().unwrap_or(""),
                    status.region.as_deref().unwrap_or(""),
                    status.ip.as_deref().unwrap_or("")
                )));
            }
            Some(false) => {
                // Clear only geoblock-driven halts (so future halt reasons don't get stomped).
                let should_clear = self
                    .health
                    .halt_reason()
                    .as_deref()
                    .map(|r| r.starts_with("geoblock_"))
                    .unwrap_or(false);
                if should_clear {
                    self.health.set_halt_reason(None);
                }
            }
            None => {
                self.health.set_halt_reason(Some(format!(
                    "geoblock_unknown error={}",
                    status.error.as_deref().unwrap_or("unknown")
                )));
            }
        }
    }

    fn apply_order_seed(&mut self, seed: OrderSeed) {
        if let Some(state) = self.markets.get_mut(&seed.slug) {
            if !state.orders.live.is_empty() {
                tracing::warn!(
                    target: "state_manager",
                    slug = %seed.slug,
                    existing = state.orders.live.len(),
                    incoming = seed.orders.len(),
                    "order seed skipped; existing live orders present"
                );
                return;
            }
            for order in seed.orders {
                state.orders.upsert(order);
            }
        } else {
            tracing::debug!(
                target: "state_manager",
                slug = %seed.slug,
                "order seed ignored; market not tracked"
            );
        }
    }

    fn apply_inventory_seed(&mut self, seed: InventorySeed) {
        if let Some(state) = self.markets.get_mut(&seed.slug) {
            if seed.ts_ms < state.inventory.last_trade_ms {
                tracing::debug!(
                    target: "state_manager",
                    slug = %seed.slug,
                    seed_ts = seed.ts_ms,
                    last_trade_ms = state.inventory.last_trade_ms,
                    "inventory seed ignored; newer fills observed"
                );
                return;
            }
            state.inventory.up = seed.up;
            state.inventory.down = seed.down;
            state.inventory.last_trade_ms = seed.ts_ms;
        } else {
            tracing::debug!(
                target: "state_manager",
                slug = %seed.slug,
                "inventory seed ignored; market not tracked"
            );
        }
    }

    fn apply_order_update(&mut self, update: OrderUpdate) {
        match update {
            OrderUpdate::Upsert { slug, order } => {
                if let Some(state) = self.markets.get_mut(&slug) {
                    state.orders.upsert(order);
                } else {
                    tracing::debug!(
                        target: "state_manager",
                        slug = %slug,
                        "order update for unknown market"
                    );
                }
            }
            OrderUpdate::Remove {
                slug,
                token_id,
                side,
                level,
                order_id,
                ..
            } => {
                if let Some(state) = self.markets.get_mut(&slug) {
                    let key = (token_id.clone(), side, level);
                    if let Some(existing) = state.orders.live.get(&key) {
                        if existing.order_id != order_id {
                            tracing::debug!(
                                target: "state_manager",
                                slug = %slug,
                                token_id = %token_id,
                                side = ?side,
                                level,
                                order_id = %order_id,
                                existing_id = %existing.order_id,
                                "order removal ignored due to id mismatch"
                            );
                            return;
                        }
                    }
                    state.orders.remove_exact(&token_id, side, level);
                } else {
                    tracing::debug!(
                        target: "state_manager",
                        slug = %slug,
                        "order removal for unknown market"
                    );
                }
            }
        }
    }

    fn apply_onchain_merge(&mut self, condition_id: &str, qty_base_units: u64) {
        if qty_base_units == 0 {
            return;
        }
        let shares = (qty_base_units as f64) / USDC_BASE_UNITS_F64;
        for state in self.markets.values_mut() {
            if state.identity.condition_id == condition_id {
                state.inventory.apply_merge(shares);
                return;
            }
        }
    }

    fn apply_onchain_redeem(&mut self, condition_id: &str) {
        for state in self.markets.values_mut() {
            if state.identity.condition_id == condition_id {
                state.inventory.apply_redeem();
                return;
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

fn market_exposure_usdc(state: &MarketState) -> (f64, f64, f64) {
    let inventory_usdc =
        (state.inventory.up.notional_usdc + state.inventory.down.notional_usdc).max(0.0);
    let mut open_order_usdc = 0.0;
    for order in state.orders.live.values() {
        if order.side != OrderSide::Buy {
            continue;
        }
        if !order.price.is_finite() || !order.remaining.is_finite() {
            continue;
        }
        if order.remaining <= 0.0 {
            continue;
        }
        open_order_usdc += order.price * order.remaining;
    }
    let total = inventory_usdc + open_order_usdc;
    (inventory_usdc, open_order_usdc, total)
}

type BookSnapshot = (Option<f64>, Option<f64>, f64, Option<f64>, Option<f64>, f64);

fn book_snapshot(state: &MarketState) -> BookSnapshot {
    (
        state.up_book.best_bid,
        state.up_book.best_ask,
        state.up_book.tick_size,
        state.down_book.best_bid,
        state.down_book.best_ask,
        state.down_book.tick_size,
    )
}

fn market_under_floor(state: &MarketState, min_quote_price: f64) -> bool {
    let floor = min_quote_price.max(0.0);
    observed_market_price(&state.up_book).is_some_and(|p| p < floor)
        || observed_market_price(&state.down_book).is_some_and(|p| p < floor)
}

fn observed_market_price(book: &crate::state::book::TokenBookTop) -> Option<f64> {
    match (book.best_bid, book.best_ask) {
        (Some(bid), Some(ask)) => Some(bid.min(ask)),
        (Some(bid), None) => Some(bid),
        (None, Some(ask)) => Some(ask),
        (None, None) => None,
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
    use tokio::sync::mpsc;
    use tokio::time::{timeout, Duration};

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

    fn make_identity_with_flags(
        active: bool,
        accepting_orders: bool,
        closed: bool,
        restricted: bool,
    ) -> MarketIdentity {
        MarketIdentity {
            active,
            accepting_orders,
            closed,
            restricted,
            ..make_identity()
        }
    }

    fn make_identity_named(
        slug: &str,
        condition_id: &str,
        token_up: &str,
        token_down: &str,
    ) -> MarketIdentity {
        MarketIdentity {
            slug: slug.to_string(),
            condition_id: condition_id.to_string(),
            token_up: token_up.to_string(),
            token_down: token_down.to_string(),
            ..make_identity()
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
                side: OrderSide::Buy,
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
    fn markout_cooldown_triggers_on_large_negative_markout() {
        let ops = OpsState::new(&AppConfig::default());
        let mut sm = StateManager::new(
            AlphaConfig::default(),
            OracleConfig::default(),
            TradingConfig::default(),
            ops.health,
            ops.metrics,
        );
        sm.apply_event(AppEvent::MarketDiscovered(make_identity()), 0);

        // Seed BTC price.
        sm.apply_event(
            AppEvent::RTDSUpdate(RTDSUpdate {
                source: RTDSSource::BinanceBtcUsdt,
                price: 10_000.0,
                ts_ms: 0,
            }),
            0,
        );

        // Fill UP (BUY) with BTC at 10_000.
        sm.apply_event(
            AppEvent::UserWsUpdate(UserWsUpdate::Fill(FillEvent {
                token_id: "up".to_string(),
                side: OrderSide::Buy,
                price: 0.49,
                shares: 1.0,
                ts_ms: 1,
            })),
            1,
        );

        // 1s later BTC is down ~21 bps => hard negative markout triggers cooldown.
        sm.apply_event(
            AppEvent::RTDSUpdate(RTDSUpdate {
                source: RTDSSource::BinanceBtcUsdt,
                price: 9_979.0,
                ts_ms: 1_001,
            }),
            1_001,
        );
        sm.apply_event(AppEvent::TimerTick { now_ms: 1_001 }, 1_001);

        let state = sm.market_state("btc-updown-15m-0").unwrap();
        assert!(
            state.alpha.maker_cooldown_until_ms >= 4_001,
            "cooldown_until_ms={}",
            state.alpha.maker_cooldown_until_ms
        );
        assert!(
            state.alpha.last_markout_bps_short.is_some(),
            "expected short-horizon markout to be computed"
        );
    }

    #[test]
    fn pair_protection_level_increases_when_unpaired_persists() {
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
                side: OrderSide::Buy,
                price: 0.49,
                shares: 10.0,
                ts_ms: 1_000,
            })),
            1_000,
        );

        sm.apply_event(AppEvent::TimerTick { now_ms: 1_000 }, 1_000);
        sm.apply_event(AppEvent::TimerTick { now_ms: 25_000 }, 25_000);

        let state = sm.market_state("btc-updown-15m-0").unwrap();
        assert_eq!(state.alpha.unpaired_since_ms, Some(1_000));
        assert!(state.alpha.unpaired_duration_s >= 24.0);
        assert!(
            state.alpha.pair_protection_level > 0.0,
            "pair_protection_level={}",
            state.alpha.pair_protection_level
        );
        assert!(state.alpha.pair_ratio < 0.85);
    }

    #[test]
    fn applying_sell_fill_reduces_cost_basis_pro_rata() {
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
                side: OrderSide::Buy,
                price: 0.4,
                shares: 10.0,
                ts_ms: 1_000,
            })),
            1_000,
        );
        sm.apply_event(
            AppEvent::UserWsUpdate(UserWsUpdate::Fill(FillEvent {
                token_id: "up".to_string(),
                side: OrderSide::Sell,
                price: 0.6,
                shares: 4.0,
                ts_ms: 2_000,
            })),
            2_000,
        );

        let state = sm.market_state("btc-updown-15m-0").unwrap();
        assert_eq!(state.inventory.up.shares, 6.0);
        assert!((state.inventory.up.notional_usdc - 2.4).abs() < 1e-12);
        let avg = state.inventory.up.avg_cost().unwrap();
        assert!((avg - 0.4).abs() < 1e-12, "avg_cost={avg}");
        assert_eq!(state.inventory.last_trade_ms, 2_000);
    }

    #[tokio::test]
    async fn fill_emits_quote_tick_only_for_affected_market() {
        let ops = OpsState::new(&AppConfig::default());
        let sm = StateManager::new(
            AlphaConfig::default(),
            OracleConfig::default(),
            TradingConfig::default(),
            ops.health,
            ops.metrics,
        );

        let (tx_events, rx_events) = mpsc::channel(8);
        let (tx_quote, mut rx_quote) = mpsc::channel(8);
        let handle = tokio::spawn(sm.run(rx_events, tx_quote));

        let t0 = now_ms();
        tx_events
            .send(AppEvent::MarketDiscovered(make_identity_named(
                "btc-updown-15m-0",
                "cond-0",
                "up-0",
                "down-0",
            )))
            .await
            .expect("send market discovered");
        tx_events
            .send(AppEvent::MarketDiscovered(make_identity_named(
                "btc-updown-15m-1",
                "cond-1",
                "up-1",
                "down-1",
            )))
            .await
            .expect("send market discovered");

        tx_events
            .send(AppEvent::UserWsUpdate(UserWsUpdate::Fill(FillEvent {
                token_id: "up-1".to_string(),
                side: OrderSide::Buy,
                price: 0.49,
                shares: 3.0,
                ts_ms: t0 + 10,
            })))
            .await
            .expect("send fill");

        let tick = timeout(Duration::from_millis(200), rx_quote.recv())
            .await
            .expect("quote tick timeout")
            .expect("quote tick");
        assert_eq!(tick.slug, "btc-updown-15m-1");
        assert_eq!(tick.state.inventory.up.shares, 3.0);

        // Ensure no quote tick for the other market was emitted by this fill.
        let no_second_tick = timeout(Duration::from_millis(50), rx_quote.recv()).await;
        assert!(
            no_second_tick.is_err(),
            "unexpected extra quote tick: {no_second_tick:?}"
        );

        drop(tx_events);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn market_ws_update_emits_quote_tick_only_for_affected_market() {
        let ops = OpsState::new(&AppConfig::default());
        let sm = StateManager::new(
            AlphaConfig::default(),
            OracleConfig::default(),
            TradingConfig::default(),
            ops.health,
            ops.metrics,
        );

        let (tx_events, rx_events) = mpsc::channel(8);
        let (tx_quote, mut rx_quote) = mpsc::channel(8);
        let handle = tokio::spawn(sm.run(rx_events, tx_quote));

        let t0 = now_ms();
        tx_events
            .send(AppEvent::MarketDiscovered(make_identity_named(
                "btc-updown-15m-0",
                "cond-0",
                "up-0",
                "down-0",
            )))
            .await
            .expect("send market discovered");
        tx_events
            .send(AppEvent::MarketDiscovered(make_identity_named(
                "btc-updown-15m-1",
                "cond-1",
                "up-1",
                "down-1",
            )))
            .await
            .expect("send market discovered");

        tx_events
            .send(AppEvent::MarketWsUpdate(MarketWsUpdate {
                token_id: "up-1".to_string(),
                best_bid: Some(0.49),
                best_ask: Some(0.51),
                tick_size: Some(0.01),
                ts_ms: t0 + 10,
            }))
            .await
            .expect("send market update");

        let tick = timeout(Duration::from_millis(200), rx_quote.recv())
            .await
            .expect("quote tick timeout")
            .expect("quote tick");
        assert_eq!(tick.slug, "btc-updown-15m-1");

        // Ensure no quote tick for the other market was emitted by this update.
        let no_second_tick = timeout(Duration::from_millis(50), rx_quote.recv()).await;
        assert!(
            no_second_tick.is_err(),
            "unexpected extra quote tick: {no_second_tick:?}"
        );

        drop(tx_events);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn fill_tick_drop_does_not_throttle_future_fill_ticks() {
        let ops = OpsState::new(&AppConfig::default());
        let sm = StateManager::new(
            AlphaConfig::default(),
            OracleConfig::default(),
            TradingConfig::default(),
            ops.health,
            ops.metrics,
        );

        let (tx_events, rx_events) = mpsc::channel(8);
        // Capacity 1 so we can intentionally fill it.
        let (tx_quote, mut rx_quote) = mpsc::channel(1);
        let handle = tokio::spawn(sm.run(rx_events, tx_quote));

        let t0 = now_ms();
        tx_events
            .send(AppEvent::MarketDiscovered(make_identity_named(
                "btc-updown-15m-0",
                "cond-0",
                "up-0",
                "down-0",
            )))
            .await
            .expect("send market discovered");

        // Fill the quote channel via a timer tick.
        tx_events
            .send(AppEvent::TimerTick { now_ms: t0 + 1 })
            .await
            .expect("send timer tick");
        let _ = timeout(Duration::from_millis(200), rx_quote.recv())
            .await
            .expect("expected timer tick quote")
            .expect("channel open");

        // Put it back to full.
        tx_events
            .send(AppEvent::TimerTick { now_ms: t0 + 2 })
            .await
            .expect("send timer tick");
        let timer_tick = timeout(Duration::from_millis(200), rx_quote.recv())
            .await
            .expect("expected timer tick quote")
            .expect("channel open");
        assert_eq!(timer_tick.slug, "btc-updown-15m-0");

        // Now the channel is empty again; refill it manually so the next fill-triggered tick drops.
        // (We can't access tx_quote directly here, so do another timer tick and *don't* recv it.)
        tx_events
            .send(AppEvent::TimerTick { now_ms: t0 + 3 })
            .await
            .expect("send timer tick");

        // Fill event should attempt to emit a fill-triggered tick, but the channel is full so it drops.
        tx_events
            .send(AppEvent::UserWsUpdate(UserWsUpdate::Fill(FillEvent {
                token_id: "up-0".to_string(),
                side: OrderSide::Buy,
                price: 0.49,
                shares: 1.0,
                ts_ms: t0 + 10,
            })))
            .await
            .expect("send fill");

        // Drain the timer tick we left in the channel.
        let _ = timeout(Duration::from_millis(200), rx_quote.recv())
            .await
            .expect("expected timer tick quote")
            .expect("channel open");

        // Send another fill within the 25ms throttle window. This should still emit a fill tick
        // because the previous attempt dropped and should not have updated the throttle timestamp.
        tx_events
            .send(AppEvent::UserWsUpdate(UserWsUpdate::Fill(FillEvent {
                token_id: "up-0".to_string(),
                side: OrderSide::Buy,
                price: 0.49,
                shares: 1.0,
                ts_ms: t0 + 11,
            })))
            .await
            .expect("send fill");

        let tick = timeout(Duration::from_millis(200), rx_quote.recv())
            .await
            .expect("quote tick timeout")
            .expect("quote tick");
        assert_eq!(tick.slug, "btc-updown-15m-0");
        assert_eq!(tick.state.inventory.up.shares, 2.0);

        drop(tx_events);
        let _ = handle.await;
    }

    #[test]
    fn heartbeat_updates_health_without_inventory_mutation() {
        let ops = OpsState::new(&AppConfig::default());
        let mut sm = StateManager::new(
            AlphaConfig::default(),
            OracleConfig::default(),
            TradingConfig::default(),
            ops.health.clone(),
            ops.metrics,
        );
        sm.apply_event(AppEvent::MarketDiscovered(make_identity()), 0);

        sm.apply_event(
            AppEvent::UserWsUpdate(UserWsUpdate::Heartbeat { ts_ms: 1_000 }),
            1_000,
        );

        let state = sm.market_state("btc-updown-15m-0").unwrap();
        assert_eq!(state.inventory.up.shares, 0.0);
        assert_eq!(state.inventory.down.shares, 0.0);

        let report = sm.health.report(1_000);
        assert_eq!(report.feeds.user_ws.last_update_ms, 1_000);
        assert_eq!(report.feeds.user_ws.status, "healthy");
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

    #[test]
    fn onchain_merge_updates_inventory_proportionally() {
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
                side: OrderSide::Buy,
                price: 0.4,
                shares: 10.0,
                ts_ms: 1_000,
            })),
            1_000,
        );
        sm.apply_event(
            AppEvent::UserWsUpdate(UserWsUpdate::Fill(FillEvent {
                token_id: "down".to_string(),
                side: OrderSide::Buy,
                price: 0.6,
                shares: 10.0,
                ts_ms: 1_000,
            })),
            1_000,
        );

        sm.apply_event(
            AppEvent::OnchainMerge {
                condition_id: "cond".to_string(),
                qty_base_units: 5_000_000,
                ts_ms: 2_000,
            },
            2_000,
        );

        let state = sm.market_state("btc-updown-15m-0").unwrap();
        assert_eq!(state.inventory.up.shares, 5.0);
        assert!((state.inventory.up.notional_usdc - 2.0).abs() < 1e-12);
        assert_eq!(state.inventory.down.shares, 5.0);
        assert!((state.inventory.down.notional_usdc - 3.0).abs() < 1e-12);
    }

    #[test]
    fn onchain_redeem_clears_inventory() {
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
                side: OrderSide::Buy,
                price: 0.45,
                shares: 4.0,
                ts_ms: 1_000,
            })),
            1_000,
        );

        sm.apply_event(
            AppEvent::OnchainRedeem {
                condition_id: "cond".to_string(),
                ts_ms: 2_000,
            },
            2_000,
        );

        let state = sm.market_state("btc-updown-15m-0").unwrap();
        assert_eq!(state.inventory.up.shares, 0.0);
        assert_eq!(state.inventory.up.notional_usdc, 0.0);
        assert_eq!(state.inventory.down.shares, 0.0);
        assert_eq!(state.inventory.down.notional_usdc, 0.0);
    }

    #[test]
    fn order_update_upsert_and_remove() {
        let ops = OpsState::new(&AppConfig::default());
        let mut sm = StateManager::new(
            AlphaConfig::default(),
            OracleConfig::default(),
            TradingConfig::default(),
            ops.health,
            ops.metrics,
        );
        sm.apply_event(AppEvent::MarketDiscovered(make_identity()), 0);

        let order = LiveOrder {
            order_id: "order-1".to_string(),
            token_id: "up".to_string(),
            side: OrderSide::Buy,
            level: 0,
            price: 0.48,
            size: 5.0,
            remaining: 5.0,
            status: crate::state::order_state::OrderStatus::Open,
            last_update_ms: 1_000,
        };

        sm.apply_event(
            AppEvent::OrderUpdate(OrderUpdate::Upsert {
                slug: "btc-updown-15m-0".to_string(),
                order: order.clone(),
            }),
            1_000,
        );

        let state = sm.market_state("btc-updown-15m-0").unwrap();
        let live = state
            .orders
            .live
            .get(&(order.token_id.clone(), order.side, order.level))
            .expect("live order stored");
        assert_eq!(live.order_id, "order-1");

        sm.apply_event(
            AppEvent::OrderUpdate(OrderUpdate::Remove {
                slug: "btc-updown-15m-0".to_string(),
                token_id: "up".to_string(),
                side: OrderSide::Buy,
                level: 0,
                order_id: "order-1".to_string(),
                ts_ms: 2_000,
            }),
            2_000,
        );

        let state = sm.market_state("btc-updown-15m-0").unwrap();
        assert!(state.orders.live.is_empty());
    }

    #[test]
    fn quoting_disabled_when_user_ws_stale() {
        let mut cfg = AppConfig::default();
        cfg.trading.dry_run = false;
        let ops = OpsState::new(&cfg);
        let mut sm = StateManager::new(
            AlphaConfig::default(),
            OracleConfig::default(),
            cfg.trading.clone(),
            ops.health.clone(),
            ops.metrics,
        );
        sm.apply_event(AppEvent::MarketDiscovered(make_identity()), 0);
        sm.apply_event(
            AppEvent::MarketWsUpdate(MarketWsUpdate {
                token_id: "up".to_string(),
                best_bid: Some(0.49),
                best_ask: Some(0.51),
                tick_size: Some(0.001),
                ts_ms: 9_500,
            }),
            9_500,
        );
        sm.apply_event(
            AppEvent::RTDSUpdate(RTDSUpdate {
                source: RTDSSource::ChainlinkBtcUsd,
                price: 40_000.0,
                ts_ms: 9_500,
            }),
            9_500,
        );
        sm.health.mark_user_ws(0);

        sm.apply_event(AppEvent::TimerTick { now_ms: 10_000 }, 10_000);

        let state = sm.market_state("btc-updown-15m-0").unwrap();
        assert!(!state.quoting_enabled);

        let report = sm.health.report(10_000);
        assert_eq!(report.quoting_enabled_markets, 0);
        assert_eq!(
            report.quoting_block_reason,
            Some("user_ws_stale".to_string())
        );
    }

    #[test]
    fn quoting_disabled_when_market_ws_stale_both_tokens() {
        let ops = OpsState::new(&AppConfig::default());
        let alpha_cfg = AlphaConfig {
            market_ws_stale_ms: 500,
            ..AlphaConfig::default()
        };
        let mut sm = StateManager::new(
            alpha_cfg,
            OracleConfig::default(),
            TradingConfig::default(),
            ops.health,
            ops.metrics,
        );
        sm.apply_event(AppEvent::MarketDiscovered(make_identity()), 0);

        sm.apply_event(
            AppEvent::MarketWsUpdate(MarketWsUpdate {
                token_id: "up".to_string(),
                best_bid: Some(0.49),
                best_ask: Some(0.51),
                tick_size: Some(0.001),
                ts_ms: 1_000,
            }),
            1_000,
        );
        sm.apply_event(
            AppEvent::MarketWsUpdate(MarketWsUpdate {
                token_id: "down".to_string(),
                best_bid: Some(0.49),
                best_ask: Some(0.51),
                tick_size: Some(0.001),
                ts_ms: 1_000,
            }),
            1_000,
        );
        sm.apply_event(
            AppEvent::RTDSUpdate(RTDSUpdate {
                source: RTDSSource::ChainlinkBtcUsd,
                price: 40_000.0,
                ts_ms: 2_000,
            }),
            2_000,
        );

        sm.apply_event(AppEvent::TimerTick { now_ms: 2_000 }, 2_000);

        let state = sm.market_state("btc-updown-15m-0").unwrap();
        assert_eq!(state.alpha.size_scalar, 0.0);
        assert!(!state.quoting_enabled);
    }

    #[test]
    fn market_exposure_includes_inventory_and_open_orders() {
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
                side: OrderSide::Buy,
                price: 0.5,
                shares: 2.0,
                ts_ms: 1_000,
            })),
            1_000,
        );
        sm.apply_event(
            AppEvent::UserWsUpdate(UserWsUpdate::Fill(FillEvent {
                token_id: "down".to_string(),
                side: OrderSide::Buy,
                price: 0.25,
                shares: 4.0,
                ts_ms: 1_000,
            })),
            1_000,
        );
        sm.apply_event(
            AppEvent::OrderUpdate(OrderUpdate::Upsert {
                slug: "btc-updown-15m-0".to_string(),
                order: LiveOrder {
                    order_id: "order-1".to_string(),
                    token_id: "up".to_string(),
                    side: OrderSide::Buy,
                    level: 0,
                    price: 0.4,
                    size: 5.0,
                    remaining: 5.0,
                    status: crate::state::order_state::OrderStatus::Open,
                    last_update_ms: 1_100,
                },
            }),
            1_100,
        );

        let state = sm.market_state("btc-updown-15m-0").unwrap();
        let (inventory_usdc, open_order_usdc, total_usdc) = market_exposure_usdc(state);
        assert!(
            (inventory_usdc - 2.0).abs() < 1e-12,
            "inventory={inventory_usdc}"
        );
        assert!(
            (open_order_usdc - 2.0).abs() < 1e-12,
            "open={open_order_usdc}"
        );
        assert!((total_usdc - 4.0).abs() < 1e-12, "total={total_usdc}");
    }

    #[test]
    fn quoting_disabled_when_exposure_over_cap() {
        let mut cfg = AppConfig::default();
        cfg.trading.max_usdc_exposure_per_market = 1.0;
        let ops = OpsState::new(&cfg);
        let mut sm = StateManager::new(
            AlphaConfig::default(),
            OracleConfig::default(),
            cfg.trading.clone(),
            ops.health.clone(),
            ops.metrics,
        );
        sm.apply_event(AppEvent::MarketDiscovered(make_identity()), 0);

        sm.apply_event(
            AppEvent::MarketWsUpdate(MarketWsUpdate {
                token_id: "up".to_string(),
                best_bid: Some(0.49),
                best_ask: Some(0.51),
                tick_size: Some(0.001),
                ts_ms: 9_000,
            }),
            9_000,
        );
        sm.apply_event(
            AppEvent::MarketWsUpdate(MarketWsUpdate {
                token_id: "down".to_string(),
                best_bid: Some(0.49),
                best_ask: Some(0.51),
                tick_size: Some(0.001),
                ts_ms: 9_000,
            }),
            9_000,
        );
        sm.apply_event(
            AppEvent::RTDSUpdate(RTDSUpdate {
                source: RTDSSource::ChainlinkBtcUsd,
                price: 40_000.0,
                ts_ms: 9_000,
            }),
            9_000,
        );
        sm.apply_event(
            AppEvent::UserWsUpdate(UserWsUpdate::Fill(FillEvent {
                token_id: "up".to_string(),
                side: OrderSide::Buy,
                price: 0.6,
                shares: 2.0,
                ts_ms: 9_050,
            })),
            9_050,
        );

        sm.apply_event(AppEvent::TimerTick { now_ms: 9_100 }, 9_100);

        let state = sm.market_state("btc-updown-15m-0").unwrap();
        assert!(!state.quoting_enabled);
        let report = sm.health.report(9_100);
        assert_eq!(
            report.quoting_block_reason,
            Some("exposure_cap".to_string())
        );
    }

    #[test]
    fn geoblock_blocks_and_clears_halt_reason() {
        let mut cfg = AppConfig::default();
        cfg.trading.dry_run = false;
        let ops = OpsState::new(&cfg);
        let mut sm = StateManager::new(
            AlphaConfig::default(),
            OracleConfig::default(),
            cfg.trading.clone(),
            ops.health.clone(),
            ops.metrics,
        );

        sm.apply_event(
            AppEvent::GeoblockStatus(GeoblockStatus {
                blocked: Some(true),
                ip: Some("1.2.3.4".to_string()),
                country: Some("US".to_string()),
                region: Some("CA".to_string()),
                ts_ms: 1_000,
                error: None,
            }),
            1_000,
        );

        let report = sm.health.report(1_000);
        assert_eq!(report.status, "halted");
        assert!(report
            .halted_reason
            .unwrap_or_default()
            .starts_with("geoblock_"));

        sm.apply_event(
            AppEvent::GeoblockStatus(GeoblockStatus {
                blocked: Some(false),
                ip: Some("1.2.3.4".to_string()),
                country: Some("IE".to_string()),
                region: Some("L".to_string()),
                ts_ms: 2_000,
                error: None,
            }),
            2_000,
        );

        let report = sm.health.report(2_000);
        assert!(report.halted_reason.is_none());
    }

    #[test]
    fn restricted_flag_does_not_block_tradability() {
        let mut cfg = AppConfig::default();
        cfg.trading.dry_run = false;
        let ops = OpsState::new(&cfg);
        let mut sm = StateManager::new(
            AlphaConfig::default(),
            OracleConfig::default(),
            cfg.trading.clone(),
            ops.health.clone(),
            ops.metrics,
        );

        sm.apply_event(
            AppEvent::MarketDiscovered(make_identity_with_flags(true, true, false, true)),
            0,
        );
        sm.health.mark_user_ws(9_500);
        sm.apply_event(
            AppEvent::MarketWsUpdate(MarketWsUpdate {
                token_id: "up".to_string(),
                best_bid: Some(0.49),
                best_ask: Some(0.51),
                tick_size: Some(0.001),
                ts_ms: 9_500,
            }),
            9_500,
        );
        sm.apply_event(
            AppEvent::MarketWsUpdate(MarketWsUpdate {
                token_id: "down".to_string(),
                best_bid: Some(0.49),
                best_ask: Some(0.51),
                tick_size: Some(0.001),
                ts_ms: 9_500,
            }),
            9_500,
        );
        sm.apply_event(
            AppEvent::RTDSUpdate(RTDSUpdate {
                source: RTDSSource::ChainlinkBtcUsd,
                price: 40_000.0,
                ts_ms: 9_500,
            }),
            9_500,
        );

        sm.apply_event(AppEvent::TimerTick { now_ms: 10_000 }, 10_000);

        let state = sm.market_state("btc-updown-15m-0").unwrap();
        assert!(state.quoting_enabled, "restricted should not block quoting");
    }
}
