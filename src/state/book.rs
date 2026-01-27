#[derive(Debug, Clone, Copy, Default)]
pub struct TokenBookTop {
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub tick_size: f64,
    pub last_update_ms: i64,
}

impl TokenBookTop {
    pub fn with_tick_size(tick_size: f64) -> Self {
        Self {
            tick_size,
            ..Self::default()
        }
    }
}
