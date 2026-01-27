#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // read by alpha/toxicity gating once RTDS client lands
pub struct RTDSPrice {
    pub price: f64,
    pub ts_ms: i64,
}
