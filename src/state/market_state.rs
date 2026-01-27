use crate::state::book::TokenBookTop;
use crate::state::inventory::InventoryState;
use crate::state::order_state::OrderState;
use crate::state::rtds_price::RTDSPrice;

#[derive(Debug, Clone)]
#[allow(dead_code)] // referenced across tasks; not all fields are read yet
pub struct MarketIdentity {
    pub slug: String,
    /// Unix seconds (used to compute the slug).
    pub interval_start_ts: i64,
    /// Unix seconds (derived from Gamma endDate).
    pub interval_end_ts: i64,
    pub condition_id: String,
    pub token_up: String,
    pub token_down: String,

    pub active: bool,
    pub closed: bool,
    pub accepting_orders: bool,
    pub restricted: bool,
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)] // alpha module will populate/read this
pub struct AlphaState {
    pub last_update_ms: i64,
    pub var_per_s: f64,
    pub drift_per_s: f64,
    pub last_price: Option<f64>,
    pub last_ts_ms: Option<i64>,
    pub divergence_since_ms: Option<i64>,
    pub fast_move_binance_price: Option<f64>,
    pub fast_move_binance_ts_ms: Option<i64>,
    pub fast_move_chainlink_price: Option<f64>,
    pub fast_move_chainlink_ts_ms: Option<i64>,

    pub cap_up: f64,
    pub cap_down: f64,
    pub target_total: f64,
    pub size_scalar: f64,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // fields are read once alpha/strategy/execution land
pub struct MarketState {
    pub identity: MarketIdentity,

    pub up_book: TokenBookTop,
    pub down_book: TokenBookTop,

    pub start_btc_price: Option<f64>,
    pub start_price_ts_ms: Option<i64>,

    pub rtds_primary: Option<RTDSPrice>,
    pub rtds_sanity: Option<RTDSPrice>,

    pub alpha: AlphaState,

    pub orders: OrderState,
    pub inventory: InventoryState,

    pub quoting_enabled: bool,
    pub cutoff_ts_ms: i64,
}

impl MarketState {
    pub fn new(identity: MarketIdentity, cutoff_ts_ms: i64) -> Self {
        Self {
            identity,
            up_book: TokenBookTop::with_tick_size(0.01),
            down_book: TokenBookTop::with_tick_size(0.01),
            start_btc_price: None,
            start_price_ts_ms: None,
            rtds_primary: None,
            rtds_sanity: None,
            alpha: AlphaState::default(),
            orders: OrderState::default(),
            inventory: InventoryState::default(),
            quoting_enabled: true,
            cutoff_ts_ms,
        }
    }

    pub fn token_side(&self, token_id: &str) -> Option<crate::state::inventory::TokenSide> {
        if token_id == self.identity.token_up {
            Some(crate::state::inventory::TokenSide::Up)
        } else if token_id == self.identity.token_down {
            Some(crate::state::inventory::TokenSide::Down)
        } else {
            None
        }
    }
}
