pub const BTC_15M_INTERVAL_S: i64 = 15 * 60;

pub fn interval_start_s(now_s: i64) -> i64 {
    (now_s.div_euclid(BTC_15M_INTERVAL_S)) * BTC_15M_INTERVAL_S
}

#[allow(dead_code)] // kept for readability in future tasks
pub fn current_btc_15m_slug(now_s: i64) -> String {
    let start = interval_start_s(now_s);
    format!("btc-updown-15m-{start}")
}

#[allow(dead_code)] // kept for readability in future tasks
pub fn next_btc_15m_slug(now_s: i64) -> String {
    let start = interval_start_s(now_s) + BTC_15M_INTERVAL_S;
    format!("btc-updown-15m-{start}")
}

pub fn now_s() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_start_rounds_down_to_15m_boundary() {
        assert_eq!(interval_start_s(0), 0);
        assert_eq!(interval_start_s(1), 0);
        assert_eq!(interval_start_s(899), 0);
        assert_eq!(interval_start_s(900), 900);
        assert_eq!(interval_start_s(901), 900);
    }

    #[test]
    fn slug_helpers_match_interval_math() {
        let now = 1769390101;
        assert_eq!(interval_start_s(now), 1769390100);
        assert_eq!(
            current_btc_15m_slug(now),
            "btc-updown-15m-1769390100".to_string()
        );
        assert_eq!(
            next_btc_15m_slug(now),
            "btc-updown-15m-1769391000".to_string()
        );
    }
}
