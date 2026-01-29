use crate::config::{AlphaConfig, TradingConfig};

const TIME_PENALTY_SCALE_S: f64 = 300.0; // Spec doesn't define time_scale; use 5m default.

pub fn compute_target_total(
    trading: &TradingConfig,
    alpha: &AlphaConfig,
    var_per_s: f64,
    avg_spread: f64,
    time_to_cancel_s: f64,
    fast_move: bool,
) -> f64 {
    let vol_ratio = if alpha.var_ref > 0.0 {
        (var_per_s / alpha.var_ref).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let spread_ratio = if alpha.spread_ref > 0.0 {
        (avg_spread / alpha.spread_ref).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let time_ratio = if TIME_PENALTY_SCALE_S > 0.0 {
        (1.0 - (time_to_cancel_s / TIME_PENALTY_SCALE_S)).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let vol_penalty = alpha.k_vol * vol_ratio;
    let spread_penalty = alpha.k_spread * spread_ratio;
    let time_penalty = alpha.k_time * time_ratio;
    let fast_move_penalty = if fast_move { alpha.k_fast } else { 0.0 };

    let raw =
        trading.target_total_base - vol_penalty - spread_penalty - time_penalty - fast_move_penalty;

    let (lo, hi) = if trading.adaptive_target_total_enabled {
        let tox = vol_ratio
            .max(spread_ratio)
            .max(time_ratio)
            .max(if fast_move { 1.0 } else { 0.0 });
        let min = lerp(
            trading.target_total_min,
            trading.target_total_min_toxic,
            tox,
        );
        let max = lerp(
            trading.target_total_max,
            trading.target_total_max_toxic,
            tox,
        );
        (min.min(max), max.max(min))
    } else {
        (trading.target_total_min, trading.target_total_max)
    };

    raw.clamp(lo, hi)
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    if !a.is_finite() || !b.is_finite() {
        return a;
    }
    let t = if t.is_finite() {
        t.clamp(0.0, 1.0)
    } else {
        0.0
    };
    a + (b - a) * t
}

#[cfg(test)]
mod tests {
    use super::compute_target_total;
    use crate::config::{AlphaConfig, TradingConfig};

    #[test]
    fn target_total_clamps_to_bounds() {
        let trading = TradingConfig {
            target_total_base: 0.992,
            target_total_min: 0.98,
            target_total_max: 0.995,
            adaptive_target_total_enabled: false,
            ..TradingConfig::default()
        };
        let alpha = AlphaConfig {
            k_vol: 1.0,
            k_spread: 1.0,
            k_time: 1.0,
            k_fast: 1.0,
            ..AlphaConfig::default()
        };

        let low = compute_target_total(&trading, &alpha, 1.0, 1.0, 0.0, true);
        assert!(low >= trading.target_total_min - 1e-12);

        let high = compute_target_total(&trading, &alpha, 0.0, 0.0, 10_000.0, false);
        assert!(high <= trading.target_total_max + 1e-12);
    }

    #[test]
    fn adaptive_target_total_can_go_below_normal_floor() {
        let trading = TradingConfig {
            adaptive_target_total_enabled: true,
            target_total_min: 0.985,
            target_total_max: 0.995,
            target_total_min_toxic: 0.98,
            target_total_max_toxic: 0.99,
            ..TradingConfig::default()
        };
        let alpha = AlphaConfig {
            var_ref: 1.0,
            spread_ref: 1.0,
            k_vol: 1.0,
            k_spread: 0.0,
            k_time: 0.0,
            k_fast: 0.0,
            ..AlphaConfig::default()
        };

        // vol_ratio=1 => tox=1 => clamp to toxic bounds.
        let out = compute_target_total(&trading, &alpha, 1.0, 0.0, 10_000.0, false);
        assert!(out <= trading.target_total_max_toxic + 1e-12);
        assert!(out >= trading.target_total_min_toxic - 1e-12);
        assert!(out < trading.target_total_min - 1e-12);
    }
}
