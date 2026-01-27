#[allow(dead_code)] // used by taker EV checks and completion logic (T10)
pub fn taker_fee_usdc(shares: f64, price: f64) -> f64 {
    shares * price * 0.25 * (price * (1.0 - price)).powi(2)
}

#[allow(dead_code)] // convenience helper for quoting/risk math
pub fn taker_fee_per_share(price: f64) -> f64 {
    taker_fee_usdc(1.0, price)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(a: f64, b: f64, tol: f64) {
        assert!((a - b).abs() <= tol, "expected {b}, got {a} (tol {tol})");
    }

    #[test]
    fn taker_fee_midpoint() {
        let fee = taker_fee_usdc(100.0, 0.5);
        assert_close(fee, 0.78125, 1e-12);
    }

    #[test]
    fn taker_fee_symmetry_about_half() {
        let p = 0.37;
        let q = 1.0 - p;
        let n1 = taker_fee_usdc(1.0, p) / p;
        let n2 = taker_fee_usdc(1.0, q) / q;
        assert_close(n1, n2, 1e-12);
    }
}
