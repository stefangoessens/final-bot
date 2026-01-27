use crate::config::AlphaConfig;
use crate::state::market_state::AlphaState;

pub fn update(alpha: &mut AlphaState, price: f64, ts_ms: i64, cfg: &AlphaConfig) {
    if !price.is_finite() || price <= 0.0 {
        return;
    }

    let Some(last_price) = alpha.last_price else {
        set_last(alpha, price, ts_ms);
        return;
    };

    let Some(last_ts_ms) = alpha.last_ts_ms else {
        set_last(alpha, price, ts_ms);
        return;
    };

    if ts_ms <= last_ts_ms {
        set_last(alpha, price, ts_ms);
        return;
    }

    let mut dt_s = (ts_ms - last_ts_ms) as f64 / 1_000.0;
    if dt_s < 1e-3 {
        dt_s = 1e-3;
    }

    let log_return = (price / last_price).ln();

    let lambda = (-std::f64::consts::LN_2 * dt_s / cfg.vol_halflife_s.max(1e-6)).exp();
    let x = log_return / dt_s.sqrt();
    let var_next = lambda * alpha.var_per_s + (1.0 - lambda) * x * x;

    let lambda_d = (-std::f64::consts::LN_2 * dt_s / cfg.drift_halflife_s.max(1e-6)).exp();
    let drift_raw = log_return / dt_s;
    let mut drift_next = lambda_d * alpha.drift_per_s + (1.0 - lambda_d) * drift_raw;

    if cfg.drift_clamp_per_s > 0.0 {
        drift_next = drift_next.clamp(-cfg.drift_clamp_per_s, cfg.drift_clamp_per_s);
    } else {
        drift_next = 0.0;
    }

    alpha.var_per_s = var_next.max(0.0);
    alpha.drift_per_s = drift_next;
    set_last(alpha, price, ts_ms);
}

pub fn var_per_s(alpha: &AlphaState) -> f64 {
    alpha.var_per_s
}

pub fn drift_per_s(alpha: &AlphaState) -> f64 {
    alpha.drift_per_s
}

fn set_last(alpha: &mut AlphaState, price: f64, ts_ms: i64) {
    alpha.last_price = Some(price);
    alpha.last_ts_ms = Some(ts_ms);
    alpha.last_update_ms = ts_ms;
}
