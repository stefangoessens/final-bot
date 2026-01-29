use std::collections::HashMap;

use crate::state::state_manager::OrderSide;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // used by user-ws + order-manager tasks
pub enum OrderStatus {
    New,
    Open,
    PartiallyFilled,
    Filled,
    Canceled,
    Rejected,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // used by user-ws + order-manager tasks
pub struct LiveOrder {
    pub order_id: String,
    pub token_id: String,
    pub side: OrderSide,
    pub level: usize,
    pub price: f64,
    pub size: f64,
    pub remaining: f64,
    pub status: OrderStatus,
    pub last_update_ms: i64,
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)] // used by order-manager tasks
pub struct OrderState {
    pub live: HashMap<(String, OrderSide, usize), LiveOrder>,
}

impl OrderState {
    #[allow(dead_code)] // used by user-ws + order-manager tasks
    pub fn upsert(&mut self, order: LiveOrder) {
        self.live
            .insert((order.token_id.clone(), order.side, order.level), order);
    }

    #[allow(dead_code)] // used by order-manager tasks
    pub fn remove(&mut self, token_id: &str, level: usize) {
        self.live
            .remove(&(token_id.to_string(), OrderSide::Buy, level));
        self.live
            .remove(&(token_id.to_string(), OrderSide::Sell, level));
    }

    pub fn remove_exact(&mut self, token_id: &str, side: OrderSide, level: usize) {
        self.live.remove(&(token_id.to_string(), side, level));
    }
}
