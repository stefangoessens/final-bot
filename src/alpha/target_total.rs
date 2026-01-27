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
    raw.clamp(trading.target_total_min, trading.target_total_max)
}

#[cfg(test)]
mod tests {
    use super::compute_target_total;
    use crate::config::{AlphaConfig, TradingConfig};

    #[test]
    fn target_total_clamps_to_bounds() {
        let trading = TradingConfig {
            target_total_base: 0.985,
            target_total_min: 0.97,
            target_total_max: 0.99,
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
}
