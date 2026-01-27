pub const USDC_BASE_UNITS_F64: f64 = 1_000_000.0;

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

    pub fn apply_merge(&mut self, shares: f64) {
        if shares <= 0.0 || self.shares <= 0.0 {
            return;
        }
        let shares_before = self.shares;
        let qty = shares.min(shares_before);
        let ratio = qty / shares_before;
        self.shares = (shares_before - qty).max(0.0);
        self.notional_usdc *= 1.0 - ratio;
        if self.shares <= 0.0 {
            self.shares = 0.0;
            self.notional_usdc = 0.0;
        }
    }

    pub fn clear(&mut self) {
        self.shares = 0.0;
        self.notional_usdc = 0.0;
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

    pub fn apply_merge(&mut self, shares: f64) {
        self.up.apply_merge(shares);
        self.down.apply_merge(shares);
    }

    pub fn apply_redeem(&mut self) {
        self.up.clear();
        self.down.clear();
    }

    #[allow(dead_code)] // used by inventory-aware quoting and risk checks
    pub fn unpaired_shares(&self) -> f64 {
        (self.up.shares - self.down.shares).abs()
    }
}
