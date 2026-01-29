pub mod fee;

pub mod engine;
pub mod quote_engine;
pub mod reward_engine;
pub mod risk;

use crate::state::state_manager::OrderSide;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // used by QuoteEngine + OrderManager in later tasks
pub enum TimeInForce {
    Gtc,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // used by QuoteEngine + OrderManager in later tasks
pub struct DesiredOrder {
    pub token_id: String,
    pub side: OrderSide,
    pub level: usize,
    pub price: f64,
    pub size: f64,
    pub post_only: bool,
    pub tif: TimeInForce,
}
