#![allow(dead_code)]

use std::collections::HashMap;

use crate::config::TradingConfig;
use crate::state::book::TokenBookTop;
use crate::state::market_state::MarketState;
use crate::state::state_manager::OrderSide;
use crate::strategy::{DesiredOrder, TimeInForce};

#[derive(Debug, Default, Clone)]
pub struct Level0ChaseLimiter {
    last_by_token: HashMap<(String, String), ChaseState>,
}

#[derive(Debug, Clone, Copy)]
struct ChaseState {
    last_p0: f64,
    last_ts_ms: i64,
}

impl Level0ChaseLimiter {
    fn clamp_upward(
        &mut self,
        slug: &str,
        token_id: &str,
        p0: f64,
        tick: f64,
        now_ms: i64,
        max_up_ticks_per_s: f64,
    ) -> f64 {
        if slug.is_empty() || token_id.is_empty() {
            return p0;
        }
        if !p0.is_finite() || p0 <= 0.0 || tick <= 0.0 || now_ms <= 0 {
            return p0;
        }

        let key = (slug.to_string(), token_id.to_string());
        let entry = self.last_by_token.entry(key).or_insert(ChaseState {
            last_p0: p0,
            last_ts_ms: now_ms,
        });

        if now_ms <= entry.last_ts_ms {
            entry.last_p0 = p0;
            entry.last_ts_ms = now_ms;
            return p0;
        }

        if p0 <= entry.last_p0 + 1e-12 {
            entry.last_p0 = p0;
            entry.last_ts_ms = now_ms;
            return p0;
        }

        if !max_up_ticks_per_s.is_finite() || max_up_ticks_per_s <= 0.0 {
            // No upward chasing allowed.
            entry.last_ts_ms = now_ms;
            return entry.last_p0;
        }

        let dt_s = (now_ms.saturating_sub(entry.last_ts_ms) as f64 / 1_000.0).max(0.0);
        let allowed_ticks = (max_up_ticks_per_s * dt_s).floor();
        let allowed = (allowed_ticks.max(0.0)) * tick;
        let clamped = p0.min(entry.last_p0 + allowed);

        entry.last_p0 = clamped;
        entry.last_ts_ms = now_ms;
        clamped
    }
}

pub fn build_desired_orders(
    state: &MarketState,
    cfg: &TradingConfig,
    now_ms: i64,
    chase: Option<&mut Level0ChaseLimiter>,
) -> Vec<DesiredOrder> {
    if !state.quoting_enabled || now_ms >= state.cutoff_ts_ms {
        return Vec::new();
    }

    if cfg.markout_cooldown_enabled && now_ms < state.alpha.maker_cooldown_until_ms {
        return Vec::new();
    }

    // Maker quoting is toxic during fast moves and when oracles disagree: we get picked off.
    // Instead, pause maker quotes and rely on the completion logic (time-gated) to handle
    // any unpaired inventory as we approach cutoff.
    if state.alpha.fast_move || state.alpha.oracle_disagree {
        return Vec::new();
    }

    let tick_up = state.up_book.tick_size.max(0.0);
    let tick_down = state.down_book.tick_size.max(0.0);
    if tick_up == 0.0 || tick_down == 0.0 {
        return Vec::new();
    }

    let min_quote_price = cfg.min_quote_price.max(0.0);
    if observed_market_price(&state.up_book).is_some_and(|p| p < min_quote_price)
        || observed_market_price(&state.down_book).is_some_and(|p| p < min_quote_price)
    {
        return Vec::new();
    }

    let mut target_total = state.alpha.target_total;
    if !target_total.is_finite() || target_total <= 0.0 {
        target_total = cfg
            .target_total_base
            .clamp(cfg.target_total_min, cfg.target_total_max);
    }

    let mut cap_up = state.alpha.cap_up;
    let mut cap_down = state.alpha.cap_down;
    if !cap_up.is_finite() || cap_up < 0.0 {
        cap_up = 0.5 * target_total;
    }
    if !cap_down.is_finite() || cap_down < 0.0 {
        cap_down = 0.5 * target_total;
    }

    let unpaired = state.inventory.unpaired_shares();
    let repair_missing_token = if unpaired > 1e-9 {
        if state.inventory.up.shares > state.inventory.down.shares {
            Some(state.identity.token_down.as_str())
        } else {
            Some(state.identity.token_up.as_str())
        }
    } else {
        None
    };
    let pair_level = if cfg.pair_protection_enabled {
        state.alpha.pair_protection_level.clamp(0.0, 1.0)
    } else {
        0.0
    };

    let base_improve = cfg.base_improve_ticks.min(cfg.max_improve_ticks);
    let up_improve = if repair_missing_token.is_some_and(|t| t == state.identity.token_up.as_str())
        && pair_level > 0.0
    {
        base_improve
            .saturating_add(cfg.pair_protection_repair_extra_improve_ticks)
            .min(cfg.max_improve_ticks)
    } else {
        base_improve
    };
    let down_improve = if repair_missing_token
        .is_some_and(|t| t == state.identity.token_down.as_str())
        && pair_level > 0.0
    {
        base_improve
            .saturating_add(cfg.pair_protection_repair_extra_improve_ticks)
            .min(cfg.max_improve_ticks)
    } else {
        base_improve
    };

    let mut up_p0 = top_price(&state.up_book, cap_up, cfg, up_improve);
    let mut down_p0 = top_price(&state.down_book, cap_down, cfg, down_improve);

    if cfg.chase_limiter_enabled {
        if let Some(limiter) = chase {
            let max_up = if unpaired > 1e-9 {
                cfg.chase_level0_max_up_ticks_per_s_repair
            } else {
                cfg.chase_level0_max_up_ticks_per_s
            };
            up_p0 = limiter.clamp_upward(
                &state.identity.slug,
                &state.identity.token_up,
                up_p0,
                tick_up,
                now_ms,
                max_up,
            );
            down_p0 = limiter.clamp_upward(
                &state.identity.slug,
                &state.identity.token_down,
                down_p0,
                tick_down,
                now_ms,
                max_up,
            );
        }
    }

    (up_p0, down_p0) = enforce_combined_cap(
        up_p0,
        down_p0,
        tick_up,
        tick_down,
        state.up_book.best_bid,
        state.down_book.best_bid,
        target_total,
    );

    let interval_start_ms = state.identity.interval_start_ts.saturating_mul(1_000);
    let base_size = if now_ms < interval_start_ms {
        cfg.base_size_shares_next
    } else {
        cfg.base_size_shares_current
    };
    let size_scalar = state.alpha.size_scalar.clamp(0.0, 1.0);
    let mut base_size = (base_size * size_scalar).max(cfg.min_order_size_shares);
    if pair_level > 0.0 {
        let min_scale = cfg.pair_protection_size_scale_min.clamp(0.0, 1.0);
        let scale = (1.0 - pair_level) + pair_level * min_scale;
        base_size = (base_size * scale).max(cfg.min_order_size_shares);
    }

    let (mut ladder_levels, ladder_step_ticks) = if cfg.adaptive_ladder_enabled {
        let toxic = state.alpha.vol_ratio >= cfg.adaptive_ladder_vol_ratio_threshold
            || state.alpha.binance_stale;
        if toxic {
            (
                cfg.ladder_levels_toxic.max(1),
                cfg.ladder_step_ticks_toxic.max(1),
            )
        } else {
            (cfg.ladder_levels, cfg.ladder_step_ticks)
        }
    } else {
        (cfg.ladder_levels, cfg.ladder_step_ticks)
    };

    if pair_level > 0.0 {
        ladder_levels = ladder_levels.min(cfg.pair_protection_max_ladder_levels.max(1));
    }

    let mut desired = Vec::with_capacity(ladder_levels.saturating_mul(2));

    build_ladder(
        &mut desired,
        &state.identity.token_up,
        OrderSide::Buy,
        up_p0,
        tick_up,
        base_size,
        ladder_levels,
        ladder_step_ticks,
        cfg,
    );
    build_ladder(
        &mut desired,
        &state.identity.token_down,
        OrderSide::Buy,
        down_p0,
        tick_down,
        base_size,
        ladder_levels,
        ladder_step_ticks,
        cfg,
    );

    if cfg.maker_sell_enabled {
        let unpaired = state.inventory.unpaired_shares();
        if unpaired >= cfg.maker_sell_trigger_unpaired_shares.max(0.0) && unpaired > 1e-9 {
            let (excess_token, excess_book, excess_tick, improve_ticks) =
                if state.inventory.up.shares > state.inventory.down.shares {
                    (
                        state.identity.token_up.as_str(),
                        &state.up_book,
                        tick_up,
                        up_improve,
                    )
                } else {
                    (
                        state.identity.token_down.as_str(),
                        &state.down_book,
                        tick_down,
                        down_improve,
                    )
                };

            let sell_p0 = top_sell_price(excess_book, cfg, improve_ticks);
            if sell_p0 > 0.0 {
                build_sell_ladder(
                    &mut desired,
                    excess_token,
                    sell_p0,
                    excess_tick,
                    base_size.min(unpaired),
                    cfg.maker_sell_levels.max(1),
                    ladder_step_ticks,
                    cfg,
                );
            }
        }
    }

    if state.inventory.unpaired_shares() <= 1e-9 {
        let mut has_up = false;
        let mut has_down = false;
        for order in &desired {
            if order.token_id == state.identity.token_up {
                has_up = true;
            }
            if order.token_id == state.identity.token_down {
                has_down = true;
            }
        }
        if has_up ^ has_down {
            return Vec::new();
        }
    }

    desired
}

#[allow(clippy::too_many_arguments)]
fn build_ladder(
    out: &mut Vec<DesiredOrder>,
    token_id: &str,
    side: OrderSide,
    p0: f64,
    tick: f64,
    base_size: f64,
    ladder_levels: usize,
    ladder_step_ticks: u64,
    cfg: &TradingConfig,
) {
    if p0 <= 0.0 || tick <= 0.0 {
        return;
    }

    let step = ladder_step_ticks as f64 * tick;
    for level in 0..ladder_levels {
        let decay = if level == 0 {
            1.0
        } else {
            cfg.size_decay.powi(level as i32)
        };
        let size = (base_size * decay).max(cfg.min_order_size_shares);
        if size <= 0.0 {
            continue;
        }

        let raw_price = p0 - (level as f64) * step;
        let price = floor_to_tick(raw_price.max(0.0), tick);
        if price <= 0.0 {
            continue;
        }
        if price < cfg.min_quote_price {
            break;
        }

        out.push(DesiredOrder {
            token_id: token_id.to_string(),
            side,
            level,
            price,
            size,
            post_only: true,
            tif: TimeInForce::Gtc,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn build_sell_ladder(
    out: &mut Vec<DesiredOrder>,
    token_id: &str,
    p0: f64,
    tick: f64,
    base_size: f64,
    ladder_levels: usize,
    ladder_step_ticks: u64,
    cfg: &TradingConfig,
) {
    if p0 <= 0.0 || tick <= 0.0 {
        return;
    }

    let step = ladder_step_ticks as f64 * tick;
    for level in 0..ladder_levels {
        let decay = if level == 0 {
            1.0
        } else {
            cfg.size_decay.powi(level as i32)
        };
        let size = (base_size * decay).max(cfg.min_order_size_shares);
        if size <= 0.0 {
            continue;
        }

        let raw_price = p0 + (level as f64) * step;
        let price = ceil_to_tick(raw_price.max(0.0), tick);
        if price <= 0.0 {
            continue;
        }
        if price < cfg.min_quote_price {
            continue;
        }

        out.push(DesiredOrder {
            token_id: token_id.to_string(),
            side: OrderSide::Sell,
            level,
            price,
            size,
            post_only: true,
            tif: TimeInForce::Gtc,
        });
    }
}

fn top_price(book: &TokenBookTop, cap: f64, cfg: &TradingConfig, improve_ticks: u64) -> f64 {
    let tick = book.tick_size;
    if tick <= 0.0 {
        return 0.0;
    }

    let improve_ticks = improve_ticks.min(cfg.max_improve_ticks);
    let improve = improve_ticks as f64 * tick;
    let mut comp = if let Some(bid) = book.best_bid {
        bid + improve
    } else if let Some(ask) = book.best_ask {
        ask - cfg.min_ticks_from_ask as f64 * tick
    } else {
        0.0
    };

    if let Some(ask) = book.best_ask {
        let post_only_cap = (ask - cfg.min_ticks_from_ask as f64 * tick).max(0.0);
        comp = comp.min(post_only_cap);
    } else {
        if let Some(bid) = book.best_bid {
            comp = bid;
        }
        comp = comp.min(0.99);
    }

    let capped = comp.min(cap).min(0.99);
    floor_to_tick(capped.max(0.0), tick)
}

fn top_sell_price(book: &TokenBookTop, cfg: &TradingConfig, improve_ticks: u64) -> f64 {
    let tick = book.tick_size;
    if tick <= 0.0 {
        return 0.0;
    }

    let improve_ticks = improve_ticks.min(cfg.max_improve_ticks);
    let improve = improve_ticks as f64 * tick;

    let mut ask = if let Some(best_ask) = book.best_ask {
        // More aggressive asks are LOWER; undercut by improve ticks.
        best_ask - improve
    } else if let Some(bid) = book.best_bid {
        bid + (cfg.min_ticks_from_ask.max(1) as f64) * tick
    } else {
        0.0
    };

    if let Some(bid) = book.best_bid {
        let post_only_floor = bid + (cfg.min_ticks_from_ask.max(1) as f64) * tick;
        ask = ask.max(post_only_floor);
    }

    let capped = ask.min(0.99);
    ceil_to_tick(capped.max(0.0), tick)
}

fn observed_market_price(book: &TokenBookTop) -> Option<f64> {
    match (book.best_bid, book.best_ask) {
        (Some(bid), Some(ask)) => Some(bid.min(ask)),
        (Some(bid), None) => Some(bid),
        (None, Some(ask)) => Some(ask),
        (None, None) => None,
    }
}

fn enforce_combined_cap(
    mut up: f64,
    mut down: f64,
    tick_up: f64,
    tick_down: f64,
    up_best_bid: Option<f64>,
    down_best_bid: Option<f64>,
    target_total: f64,
) -> (f64, f64) {
    if tick_up <= 0.0 || tick_down <= 0.0 {
        return (up, down);
    }

    let mut toggle = false;
    let mut iter = 0;
    while up + down > target_total + 1e-12 && iter < 10_000 {
        let up_delta = up_best_bid.map(|b| (up - b).max(0.0)).unwrap_or(up);
        let down_delta = down_best_bid.map(|b| (down - b).max(0.0)).unwrap_or(down);

        let reduce_up = if up <= 0.0 {
            false
        } else if down <= 0.0 {
            true
        } else if (up_delta - down_delta).abs() > 1e-12 {
            up_delta >= down_delta
        } else {
            toggle = !toggle;
            toggle
        };

        if reduce_up {
            up = floor_to_tick((up - tick_up).max(0.0), tick_up);
        } else {
            down = floor_to_tick((down - tick_down).max(0.0), tick_down);
        }

        iter += 1;
    }
    (up, down)
}

fn floor_to_tick(price: f64, tick: f64) -> f64 {
    if tick <= 0.0 || !price.is_finite() {
        return 0.0;
    }
    // Guard against floating-point error at tick boundaries (e.g. 0.21 - 0.01 = 0.199999...).
    let eps = (tick.abs() * 1e-6).max(1e-12);
    let steps = ((price + eps) / tick).floor();
    if !steps.is_finite() {
        return 0.0;
    }
    let mut rounded = steps * tick;
    if rounded > price + eps {
        rounded = (steps - 1.0) * tick;
    }
    if !rounded.is_finite() || rounded < 0.0 {
        return 0.0;
    }
    rounded
}

fn ceil_to_tick(price: f64, tick: f64) -> f64 {
    if tick <= 0.0 || !price.is_finite() {
        return 0.0;
    }
    let eps = (tick.abs() * 1e-6).max(1e-12);
    let steps = ((price - eps) / tick).ceil();
    if !steps.is_finite() {
        return 0.0;
    }
    let mut rounded = steps * tick;
    if rounded + eps < price {
        rounded = (steps + 1.0) * tick;
    }
    if !rounded.is_finite() || rounded < 0.0 {
        return 0.0;
    }
    if rounded > 1.0 + eps {
        return 0.0;
    }
    if rounded > 1.0 && rounded <= 1.0 + eps {
        rounded = 1.0;
    }
    rounded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::market_state::{MarketIdentity, MarketState};

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
        state.up_book.tick_size = 0.01;
        state.down_book.tick_size = 0.01;
        state
    }

    #[test]
    fn combined_cap_respected_level0() {
        let mut state = base_state();
        state.up_book.best_bid = Some(0.49);
        state.up_book.best_ask = Some(0.50);
        state.down_book.best_bid = Some(0.49);
        state.down_book.best_ask = Some(0.50);
        state.alpha.target_total = 0.98;
        state.alpha.cap_up = 0.98;
        state.alpha.cap_down = 0.98;
        state.alpha.size_scalar = 1.0;

        let mut cfg = TradingConfig::default();
        cfg.base_improve_ticks = 0;

        let desired = build_desired_orders(&state, &cfg, 1000, None);
        let mut up0 = None;
        let mut down0 = None;
        for order in desired {
            if order.level == 0 && order.token_id == "up" {
                up0 = Some(order.price);
            }
            if order.level == 0 && order.token_id == "down" {
                down0 = Some(order.price);
            }
        }
        let sum = up0.unwrap_or(0.0) + down0.unwrap_or(0.0);
        assert!(
            sum <= state.alpha.target_total + 1e-12,
            "combined cap violated: {sum}"
        );
    }

    #[test]
    fn no_orders_until_tick_size_known() {
        let mut state = base_state();
        state.up_book.tick_size = 0.0;
        state.down_book.tick_size = 0.01;
        state.up_book.best_bid = Some(0.49);
        state.up_book.best_ask = Some(0.50);
        state.down_book.best_bid = Some(0.49);
        state.down_book.best_ask = Some(0.50);
        state.alpha.target_total = 0.98;
        state.alpha.cap_up = 0.98;
        state.alpha.cap_down = 0.98;
        state.alpha.size_scalar = 1.0;

        let cfg = TradingConfig::default();
        let desired = build_desired_orders(&state, &cfg, 1000, None);
        assert!(desired.is_empty(), "expected no orders before tick known");
    }

    #[test]
    fn price_aligns_to_tick() {
        let mut state = base_state();
        state.up_book.tick_size = 0.005;
        state.down_book.tick_size = 0.005;
        state.up_book.best_bid = Some(0.513);
        state.up_book.best_ask = Some(0.525);
        state.down_book.best_bid = Some(0.486);
        state.down_book.best_ask = Some(0.495);
        state.alpha.target_total = 0.99;
        state.alpha.cap_up = 0.99;
        state.alpha.cap_down = 0.99;
        state.alpha.size_scalar = 1.0;

        let mut cfg = TradingConfig::default();
        cfg.base_improve_ticks = 0;

        let desired = build_desired_orders(&state, &cfg, 1000, None);
        for order in desired {
            let tick = if order.token_id == "up" {
                state.up_book.tick_size
            } else {
                state.down_book.tick_size
            };
            let rem = (order.price / tick).fract().abs();
            assert!(
                rem < 1e-9 || (1.0 - rem) < 1e-9,
                "price not aligned to tick: price={} tick={}",
                order.price,
                tick
            );
        }
    }

    #[test]
    fn post_only_enforced() {
        let mut state = base_state();
        state.up_book.best_bid = Some(0.49);
        state.up_book.best_ask = Some(0.50);
        state.down_book.best_bid = Some(0.47);
        state.down_book.best_ask = Some(0.48);
        state.alpha.target_total = 0.99;
        state.alpha.cap_up = 0.99;
        state.alpha.cap_down = 0.99;
        state.alpha.size_scalar = 1.0;

        let mut cfg = TradingConfig::default();
        cfg.min_ticks_from_ask = 1;
        cfg.base_improve_ticks = 1;

        let desired = build_desired_orders(&state, &cfg, 1000, None);
        for order in desired {
            let (ask, tick) = if order.token_id == "up" {
                (state.up_book.best_ask.unwrap(), state.up_book.tick_size)
            } else {
                (state.down_book.best_ask.unwrap(), state.down_book.tick_size)
            };
            let max_price = ask - cfg.min_ticks_from_ask as f64 * tick;
            assert!(
                order.price <= max_price + 1e-12,
                "post-only violated: price={} ask={}",
                order.price,
                ask
            );
        }
    }

    #[test]
    fn quote_floor_gates_if_any_outcome_under_floor() {
        let cfg = TradingConfig::default();

        let mut state = base_state();
        state.up_book.best_bid = Some(0.19);
        state.up_book.best_ask = Some(0.21);
        state.down_book.best_bid = Some(0.50);
        state.down_book.best_ask = Some(0.51);
        state.alpha.target_total = 0.99;
        state.alpha.cap_up = 0.99;
        state.alpha.cap_down = 0.99;
        state.alpha.size_scalar = 1.0;

        let desired = build_desired_orders(&state, &cfg, 1000, None);
        assert!(
            desired.is_empty(),
            "expected quoting to pause when Up is under floor"
        );

        let mut state = base_state();
        state.up_book.best_bid = Some(0.50);
        state.up_book.best_ask = Some(0.51);
        state.down_book.best_bid = None;
        state.down_book.best_ask = Some(0.19);
        state.alpha.target_total = 0.99;
        state.alpha.cap_up = 0.99;
        state.alpha.cap_down = 0.99;
        state.alpha.size_scalar = 1.0;

        let desired = build_desired_orders(&state, &cfg, 1000, None);
        assert!(
            desired.is_empty(),
            "expected quoting to pause when Down is under floor"
        );
    }

    #[test]
    fn quote_floor_never_outputs_orders_below_floor() {
        let mut cfg = TradingConfig::default();
        cfg.ladder_levels = 5;

        let mut state = base_state();
        state.up_book.best_bid = Some(0.22);
        state.up_book.best_ask = Some(0.24);
        state.down_book.best_bid = Some(0.22);
        state.down_book.best_ask = Some(0.24);
        state.alpha.target_total = 0.99;
        state.alpha.cap_up = 0.99;
        state.alpha.cap_down = 0.99;
        state.alpha.size_scalar = 1.0;

        let desired = build_desired_orders(&state, &cfg, 1000, None);
        assert!(!desired.is_empty(), "expected some desired orders");
        for order in desired {
            assert!(
                order.price >= cfg.min_quote_price,
                "produced price below floor: price={} floor={}",
                order.price,
                cfg.min_quote_price
            );
        }
    }

    #[test]
    fn floor_to_tick_does_not_floor_one_tick_too_low_at_boundary() {
        let p = 0.21 - 0.01;
        let rounded = floor_to_tick(p, 0.01);
        assert!((rounded - 0.20).abs() < 1e-12, "rounded={rounded}");

        let p = 0.201 - 0.001;
        let rounded = floor_to_tick(p, 0.001);
        assert!((rounded - 0.20).abs() < 1e-12, "rounded={rounded}");

        let p = 0.2001 - 0.0001;
        let rounded = floor_to_tick(p, 0.0001);
        assert!((rounded - 0.20).abs() < 1e-12, "rounded={rounded}");
    }

    #[test]
    fn balanced_inventory_suppresses_one_sided_quotes() {
        let cfg = TradingConfig::default();

        let mut state = base_state();
        state.up_book.best_bid = Some(0.50);
        state.up_book.best_ask = Some(0.51);
        state.down_book.best_bid = Some(0.50);
        state.down_book.best_ask = Some(0.51);
        state.alpha.target_total = 0.99;
        state.alpha.cap_up = 0.0;
        state.alpha.cap_down = 0.99;
        state.alpha.size_scalar = 1.0;

        let desired = build_desired_orders(&state, &cfg, 1000, None);
        assert!(
            desired.is_empty(),
            "expected quoting to pause when balanced but only one side would quote"
        );
    }

    #[test]
    fn level0_chase_limiter_clamps_upward_moves() {
        let mut limiter = Level0ChaseLimiter::default();
        let slug = "m";
        let token = "t";
        let tick = 0.01;

        let first = limiter.clamp_upward(slug, token, 0.50, tick, 1_000, 5.0);
        assert!((first - 0.50).abs() < 1e-12);

        // 1s later at 5 ticks/s => allow 5 ticks = 0.05.
        let clamped = limiter.clamp_upward(slug, token, 0.60, tick, 2_000, 5.0);
        assert!((clamped - 0.55).abs() < 1e-12, "clamped={clamped}");

        // Downward moves should not be clamped.
        let down = limiter.clamp_upward(slug, token, 0.53, tick, 3_000, 5.0);
        assert!((down - 0.53).abs() < 1e-12, "down={down}");
    }
}
