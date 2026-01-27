#[derive(Debug, Clone, Copy, Default)]
pub struct InventorySide {
    pub shares: f64,
    pub notional_usdc: f64,
}

impl InventorySide {
    #[allow(dead_code)] // used by inventory-aware quoting and reporting
    pub fn avg_cost(&self) -> Option<f64> {
        if self.shares > 0.0 {
            Some(self.notional_usdc / self.shares)
        } else {
            None
        }
    }

    pub fn apply_buy_fill(&mut self, price: f64, shares: f64) {
        self.shares += shares;
        self.notional_usdc += price * shares;
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct InventoryState {
    pub up: InventorySide,
    pub down: InventorySide,
    pub last_trade_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSide {
    Up,
    Down,
}

impl InventoryState {
    pub fn apply_buy_fill(&mut self, side: TokenSide, price: f64, shares: f64, ts_ms: i64) {
        match side {
            TokenSide::Up => self.up.apply_buy_fill(price, shares),
            TokenSide::Down => self.down.apply_buy_fill(price, shares),
        }
        self.last_trade_ms = ts_ms;
    }

    #[allow(dead_code)] // used by inventory-aware quoting and risk checks
    pub fn unpaired_shares(&self) -> f64 {
        (self.up.shares - self.down.shares).abs()
    }
}
