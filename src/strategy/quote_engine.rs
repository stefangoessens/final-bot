#![allow(dead_code)]

use crate::config::TradingConfig;
use crate::state::book::TokenBookTop;
use crate::state::market_state::MarketState;
use crate::strategy::{DesiredOrder, TimeInForce};

pub fn build_desired_orders(
    state: &MarketState,
    cfg: &TradingConfig,
    now_ms: i64,
) -> Vec<DesiredOrder> {
    if !state.quoting_enabled || now_ms >= state.cutoff_ts_ms {
        return Vec::new();
    }

    let tick_up = state.up_book.tick_size.max(0.0);
    let tick_down = state.down_book.tick_size.max(0.0);
    if tick_up == 0.0 || tick_down == 0.0 {
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
    if !cap_up.is_finite() || cap_up <= 0.0 {
        cap_up = 0.5 * target_total;
    }
    if !cap_down.is_finite() || cap_down <= 0.0 {
        cap_down = 0.5 * target_total;
    }

    let mut up_p0 = top_price(&state.up_book, cap_up, cfg);
    let mut down_p0 = top_price(&state.down_book, cap_down, cfg);

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
    let base_size = (base_size * size_scalar).max(cfg.min_order_size_shares);

    let mut desired = Vec::with_capacity(cfg.ladder_levels.saturating_mul(2));

    build_ladder(
        &mut desired,
        &state.identity.token_up,
        up_p0,
        tick_up,
        base_size,
        cfg,
    );
    build_ladder(
        &mut desired,
        &state.identity.token_down,
        down_p0,
        tick_down,
        base_size,
        cfg,
    );

    desired
}

fn build_ladder(
    out: &mut Vec<DesiredOrder>,
    token_id: &str,
    p0: f64,
    tick: f64,
    base_size: f64,
    cfg: &TradingConfig,
) {
    if p0 <= 0.0 || tick <= 0.0 {
        return;
    }

    let step = cfg.ladder_step_ticks as f64 * tick;
    for level in 0..cfg.ladder_levels {
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

        out.push(DesiredOrder {
            token_id: token_id.to_string(),
            level,
            price,
            size,
            post_only: true,
            tif: TimeInForce::Gtc,
        });
    }
}

fn top_price(book: &TokenBookTop, cap: f64, cfg: &TradingConfig) -> f64 {
    let tick = book.tick_size;
    if tick <= 0.0 {
        return 0.0;
    }

    let improve_ticks = cfg.base_improve_ticks.min(cfg.max_improve_ticks);
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
    (price / tick).floor() * tick
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
        MarketState::new(identity, 900_000)
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

        let desired = build_desired_orders(&state, &cfg, 1000);
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

        let desired = build_desired_orders(&state, &cfg, 1000);
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

        let desired = build_desired_orders(&state, &cfg, 1000);
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
}
