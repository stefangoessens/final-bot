use crate::config::{AlphaConfig, OracleConfig};
use crate::state::market_state::AlphaState;
use crate::state::rtds_price::RTDSPrice;

use super::Regime;

pub const BINANCE_STALE_SIZE_SCALAR: f64 = 0.7;

#[derive(Debug, Clone, Copy)]
pub struct RegimeEval {
    pub regime: Regime,
    pub chainlink_stale: bool,
    pub binance_stale: bool,
    pub market_ws_stale: bool,
    pub fast_move: bool,
    pub oracle_disagree: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn evaluate_regime(
    alpha: &mut AlphaState,
    now_ms: i64,
    cfg: &AlphaConfig,
    oracle_cfg: &OracleConfig,
    rtds_primary: Option<RTDSPrice>,
    rtds_sanity: Option<RTDSPrice>,
    market_ws_stale: bool,
    var_per_s: f64,
) -> RegimeEval {
    let chainlink_stale = is_stale(now_ms, oracle_cfg.chainlink_stale_ms, rtds_sanity);
    let binance_stale = is_stale(now_ms, oracle_cfg.binance_stale_ms, rtds_primary);
    let oracle_disagree = oracle_disagree(rtds_primary, rtds_sanity, oracle_cfg);
    let fast_move = is_fast_move(
        alpha,
        cfg,
        oracle_cfg,
        rtds_primary,
        rtds_sanity,
        binance_stale,
        var_per_s,
    );

    let regime = if chainlink_stale || market_ws_stale {
        Regime::StaleOracle
    } else if fast_move {
        Regime::FastMove
    } else if binance_stale {
        Regime::StaleBinance
    } else {
        Regime::Normal
    };

    RegimeEval {
        regime,
        chainlink_stale,
        binance_stale,
        market_ws_stale,
        fast_move,
        oracle_disagree,
    }
}

fn is_stale(now_ms: i64, stale_ms: i64, price: Option<RTDSPrice>) -> bool {
    match price {
        Some(p) => now_ms.saturating_sub(p.ts_ms) > stale_ms,
        None => true,
    }
}

fn oracle_disagree(
    rtds_primary: Option<RTDSPrice>,
    rtds_sanity: Option<RTDSPrice>,
    oracle_cfg: &OracleConfig,
) -> bool {
    let (Some(primary), Some(sanity)) = (rtds_primary, rtds_sanity) else {
        return false;
    };
    let mid = (primary.price + sanity.price) / 2.0;
    if mid <= 0.0 {
        return false;
    }
    let divergence_bps = ((primary.price - sanity.price).abs() / mid) * 10_000.0;
    divergence_bps >= oracle_cfg.oracle_disagree_threshold_bps
}

fn is_fast_move(
    alpha: &mut AlphaState,
    cfg: &AlphaConfig,
    oracle_cfg: &OracleConfig,
    rtds_primary: Option<RTDSPrice>,
    rtds_sanity: Option<RTDSPrice>,
    binance_stale: bool,
    var_per_s: f64,
) -> bool {
    let return_bps = if !binance_stale {
        fast_move_return_bps(
            alpha,
            rtds_primary,
            oracle_cfg.fast_move_window_ms,
            true,
        )
    } else {
        fast_move_return_bps(
            alpha,
            rtds_sanity,
            oracle_cfg.fast_move_window_ms,
            false,
        )
    };

    let return_fast = return_bps
        .map(|bps| bps.abs() >= oracle_cfg.fast_move_threshold_bps)
        .unwrap_or(false);
    let var_fast = var_per_s > cfg.var_ref;
    return_fast || var_fast
}

fn fast_move_return_bps(
    alpha: &mut AlphaState,
    price: Option<RTDSPrice>,
    window_ms: i64,
    use_binance: bool,
) -> Option<f64> {
    let price = price?;
    if !price.price.is_finite() || price.price <= 0.0 {
        return None;
    }

    let (anchor_price, anchor_ts) = if use_binance {
        (
            &mut alpha.fast_move_binance_price,
            &mut alpha.fast_move_binance_ts_ms,
        )
    } else {
        (
            &mut alpha.fast_move_chainlink_price,
            &mut alpha.fast_move_chainlink_ts_ms,
        )
    };

    match (*anchor_price, *anchor_ts) {
        (Some(p0), Some(t0)) => {
            if price.ts_ms <= t0 {
                return None;
            }
            let dt = price.ts_ms - t0;
            if dt < window_ms {
                return None;
            }
            let return_bps = ((price.price / p0) - 1.0).abs() * 10_000.0;
            *anchor_price = Some(price.price);
            *anchor_ts = Some(price.ts_ms);
            Some(return_bps)
        }
        _ => {
            *anchor_price = Some(price.price);
            *anchor_ts = Some(price.ts_ms);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::market_state::AlphaState;

    #[test]
    fn chainlink_stale_sets_stale_oracle() {
        let mut alpha = AlphaState::default();
        let alpha_cfg = AlphaConfig::default();
        let oracle_cfg = OracleConfig::default();
        let now_ms = 1_000;
        let primary = RTDSPrice {
            price: 100.0,
            ts_ms: now_ms,
        };
        let regime = evaluate_regime(
            &mut alpha,
            now_ms,
            &alpha_cfg,
            &oracle_cfg,
            Some(primary),
            None,
            false,
            0.0,
        );
        assert_eq!(regime.regime, Regime::StaleOracle);
    }

    #[test]
    fn binance_stale_sets_stale_binance() {
        let mut alpha = AlphaState::default();
        let alpha_cfg = AlphaConfig::default();
        let oracle_cfg = OracleConfig {
            chainlink_stale_ms: 5_000,
            binance_stale_ms: 500,
            fast_move_window_ms: 1_000,
            fast_move_threshold_bps: 10.0,
            oracle_disagree_threshold_bps: 100.0,
        };
        let now_ms = 2_000;
        let chainlink = RTDSPrice {
            price: 100.0,
            ts_ms: now_ms,
        };
        let binance = RTDSPrice {
            price: 100.1,
            ts_ms: now_ms - 5_000,
        };
        let regime = evaluate_regime(
            &mut alpha,
            now_ms,
            &alpha_cfg,
            &oracle_cfg,
            Some(binance),
            Some(chainlink),
            false,
            0.0,
        );
        assert_eq!(regime.regime, Regime::StaleBinance);
    }

    #[test]
    fn oracle_disagree_is_warning_only() {
        let mut alpha = AlphaState::default();
        let alpha_cfg = AlphaConfig::default();
        let oracle_cfg = OracleConfig {
            chainlink_stale_ms: 5_000,
            binance_stale_ms: 5_000,
            fast_move_window_ms: 1_000,
            fast_move_threshold_bps: 50.0,
            oracle_disagree_threshold_bps: 5.0,
        };
        let now_ms = 2_000;
        let chainlink = RTDSPrice {
            price: 100.0,
            ts_ms: now_ms,
        };
        let binance = RTDSPrice {
            price: 100.2,
            ts_ms: now_ms,
        };
        let regime = evaluate_regime(
            &mut alpha,
            now_ms,
            &alpha_cfg,
            &oracle_cfg,
            Some(binance),
            Some(chainlink),
            false,
            0.0,
        );
        assert_eq!(regime.regime, Regime::Normal);
        assert!(regime.oracle_disagree);
    }
}
