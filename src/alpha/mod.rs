pub mod probability;
pub mod target_total;
pub mod toxicity;
pub mod volatility;

use crate::config::{AlphaConfig, OracleConfig, TradingConfig};
use crate::state::book::TokenBookTop;
use crate::state::market_state::MarketState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Regime {
    Normal,
    FastMove,
    StaleOracle,
    StaleBinance,
}

#[derive(Debug, Clone, Copy)]
pub struct AlphaOutput {
    pub regime: Regime,
    pub q_up: f64,
    pub cap_up: f64,
    pub cap_down: f64,
    pub target_total: f64,
    pub size_scalar: f64,
    pub oracle_disagree: bool,
}

pub fn update_alpha(
    market_state: &mut MarketState,
    now_ms: i64,
    alpha_cfg: &AlphaConfig,
    oracle_cfg: &OracleConfig,
    trading_cfg: &TradingConfig,
) -> AlphaOutput {
    let chainlink = market_state.rtds_sanity;
    let binance = market_state.rtds_primary;

    let chainlink_fresh = is_fresh(chainlink, oracle_cfg.chainlink_stale_ms, now_ms);

    if let Some(chainlink) = chainlink {
        let start_ms = market_state
            .identity
            .interval_start_ts
            .saturating_mul(1_000);
        if market_state.start_btc_price.is_none()
            && chainlink.ts_ms >= start_ms
            && chainlink_fresh
        {
            market_state.start_btc_price = Some(chainlink.price);
            market_state.start_price_ts_ms = Some(chainlink.ts_ms);
        }
    }

    if chainlink_fresh {
        if let Some(chainlink) = chainlink {
            volatility::update(
                &mut market_state.alpha,
                chainlink.price,
                chainlink.ts_ms,
                alpha_cfg,
            );
        }
    }

    let var_per_s = volatility::var_per_s(&market_state.alpha);
    let drift_per_s = volatility::drift_per_s(&market_state.alpha);

    let (up_stale, down_stale, market_ws_stale) =
        market_ws_staleness(market_state, now_ms, alpha_cfg.market_ws_stale_ms);

    let regime_eval = toxicity::evaluate_regime(
        &mut market_state.alpha,
        now_ms,
        alpha_cfg,
        oracle_cfg,
        binance,
        chainlink,
        market_ws_stale,
        var_per_s,
    );
    let regime = regime_eval.regime;

    let time_to_cancel_s = ((market_state.cutoff_ts_ms - now_ms).max(0) as f64) / 1_000.0;
    let avg_spread = average_spread(&market_state.up_book, &market_state.down_book).unwrap_or(0.0);

    let target_total = target_total::compute_target_total(
        trading_cfg,
        alpha_cfg,
        var_per_s,
        avg_spread,
        time_to_cancel_s,
        regime_eval.fast_move,
    );

    let q_up = compute_q_up(market_state, now_ms, drift_per_s, var_per_s);
    let q_down = (1.0 - q_up).clamp(0.0, 1.0);

    let cap_up = if up_stale { 0.0 } else { q_up * target_total };
    let cap_down = if down_stale { 0.0 } else { q_down * target_total };

    let size_scalar = compute_size_scalar(
        &regime_eval,
        target_total,
        trading_cfg.target_total_base,
    );

    market_state.alpha.cap_up = cap_up;
    market_state.alpha.cap_down = cap_down;
    market_state.alpha.target_total = target_total;
    market_state.alpha.size_scalar = size_scalar;

    AlphaOutput {
        regime,
        q_up,
        cap_up,
        cap_down,
        target_total,
        size_scalar,
        oracle_disagree: regime_eval.oracle_disagree,
    }
}

fn compute_q_up(market_state: &MarketState, now_ms: i64, drift_per_s: f64, var_per_s: f64) -> f64 {
    let Some(s0) = market_state.start_btc_price else {
        return 0.5;
    };
    let st = market_state
        .rtds_sanity
        .or(market_state.rtds_primary)
        .map(|p| p.price);
    let Some(st) = st else { return 0.5 };

    let end_ms = market_state.identity.interval_end_ts.saturating_mul(1_000);
    let tau_s = ((end_ms - now_ms).max(1) as f64) / 1_000.0;
    probability::compute_q_up(s0, st, tau_s, drift_per_s, var_per_s)
}

fn is_fresh(price: Option<crate::state::rtds_price::RTDSPrice>, stale_ms: i64, now_ms: i64) -> bool {
    match price {
        Some(p) => now_ms.saturating_sub(p.ts_ms) <= stale_ms,
        None => false,
    }
}

fn compute_size_scalar(
    regime: &toxicity::RegimeEval,
    target_total: f64,
    target_total_base: f64,
) -> f64 {
    if regime.chainlink_stale || regime.market_ws_stale {
        return 0.0;
    }
    let mut scalar = 1.0;
    if regime.fast_move && target_total_base > 0.0 {
        scalar *= (target_total / target_total_base).clamp(0.0, 1.0);
    }
    if regime.binance_stale {
        scalar *= toxicity::BINANCE_STALE_SIZE_SCALAR;
    }
    scalar.clamp(0.0, 1.0)
}

fn average_spread(up: &TokenBookTop, down: &TokenBookTop) -> Option<f64> {
    let up_spread = spread(up);
    let down_spread = spread(down);
    match (up_spread, down_spread) {
        (Some(a), Some(b)) => Some((a + b) / 2.0),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn spread(book: &TokenBookTop) -> Option<f64> {
    match (book.best_bid, book.best_ask) {
        (Some(bid), Some(ask)) if ask >= bid => Some(ask - bid),
        _ => None,
    }
}

fn market_ws_staleness(
    market_state: &MarketState,
    now_ms: i64,
    stale_ms: i64,
) -> (bool, bool, bool) {
    let up_stale = is_market_ws_stale(now_ms, stale_ms, market_state.up_book.last_update_ms);
    let down_stale = is_market_ws_stale(now_ms, stale_ms, market_state.down_book.last_update_ms);
    let all_stale = up_stale && down_stale;
    (up_stale, down_stale, all_stale)
}

fn is_market_ws_stale(now_ms: i64, stale_ms: i64, last_update_ms: i64) -> bool {
    last_update_ms > 0 && now_ms.saturating_sub(last_update_ms) > stale_ms
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AlphaConfig, OracleConfig, TradingConfig};
    use crate::state::market_state::{MarketIdentity, MarketState};
    use crate::state::rtds_price::RTDSPrice;

    fn base_identity() -> MarketIdentity {
        MarketIdentity {
            slug: "btc-updown-15m-test".to_string(),
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
    fn stale_chainlink_sets_regime_stale_oracle() {
        let identity = base_identity();
        let mut market = MarketState::new(identity, 840_000);
        market.rtds_sanity = None;

        let alpha_cfg = AlphaConfig {
            rtds_stale_ms: 500,
            ..AlphaConfig::default()
        };
        let oracle_cfg = OracleConfig {
            chainlink_stale_ms: 500,
            binance_stale_ms: 500,
            fast_move_window_ms: 500,
            fast_move_threshold_bps: 10.0,
            oracle_disagree_threshold_bps: 50.0,
        };
        let trading_cfg = TradingConfig::default();

        let out = update_alpha(&mut market, 1_000, &alpha_cfg, &oracle_cfg, &trading_cfg);
        assert_eq!(out.regime, Regime::StaleOracle);
    }

    #[test]
    fn binance_stale_sets_regime_stale_binance() {
        let identity = base_identity();
        let mut market = MarketState::new(identity, 840_000);
        let now_ms = 10_000;
        market.rtds_sanity = Some(RTDSPrice {
            price: 100.0,
            ts_ms: now_ms,
        });
        market.rtds_primary = Some(RTDSPrice {
            price: 100.5,
            ts_ms: now_ms - 5_000,
        });

        let alpha_cfg = AlphaConfig {
            ..AlphaConfig::default()
        };
        let oracle_cfg = OracleConfig {
            chainlink_stale_ms: 2_000,
            binance_stale_ms: 1_000,
            fast_move_window_ms: 500,
            fast_move_threshold_bps: 10.0,
            oracle_disagree_threshold_bps: 50.0,
        };
        let trading_cfg = TradingConfig::default();

        let out = update_alpha(&mut market, now_ms, &alpha_cfg, &oracle_cfg, &trading_cfg);
        assert_eq!(out.regime, Regime::StaleBinance);
    }

    #[test]
    fn one_token_stale_zeroes_only_that_cap() {
        let identity = base_identity();
        let mut market = MarketState::new(identity, 840_000);
        let now_ms = 10_000;
        market.up_book.last_update_ms = now_ms - 2_000;
        market.down_book.last_update_ms = now_ms;
        market.rtds_sanity = Some(RTDSPrice {
            price: 100.0,
            ts_ms: now_ms,
        });
        market.rtds_primary = Some(RTDSPrice {
            price: 100.1,
            ts_ms: now_ms,
        });

        let alpha_cfg = AlphaConfig {
            market_ws_stale_ms: 1_000,
            ..AlphaConfig::default()
        };
        let oracle_cfg = OracleConfig {
            chainlink_stale_ms: 5_000,
            binance_stale_ms: 5_000,
            fast_move_window_ms: 1_000,
            fast_move_threshold_bps: 10.0,
            oracle_disagree_threshold_bps: 50.0,
        };
        let trading_cfg = TradingConfig::default();

        let out = update_alpha(&mut market, now_ms, &alpha_cfg, &oracle_cfg, &trading_cfg);
        assert_eq!(out.cap_up, 0.0);
        assert!(out.cap_down > 0.0);
        assert!(out.size_scalar > 0.0);
    }
}
