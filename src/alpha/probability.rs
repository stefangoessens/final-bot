const MIN_VAR_PER_S: f64 = 1e-12;
const EPS_PCT: f64 = 1e-6;

pub fn compute_q_up(s0: f64, st: f64, tau_s: f64, mu: f64, var_per_s: f64) -> f64 {
    if !s0.is_finite() || !st.is_finite() || s0 <= 0.0 || st <= 0.0 {
        return 0.5;
    }

    if tau_s <= 0.0 {
        return if st > s0 {
            1.0
        } else if st < s0 {
            0.0
        } else {
            0.5
        };
    }

    if var_per_s <= MIN_VAR_PER_S {
        let upper = s0 * (1.0 + EPS_PCT);
        let lower = s0 * (1.0 - EPS_PCT);
        if st > upper {
            return 1.0;
        }
        if st < lower {
            return 0.0;
        }
        return 0.5;
    }

    let sigma = (var_per_s * tau_s).sqrt();
    if sigma <= 0.0 {
        return 0.5;
    }

    let z = (s0.ln() - st.ln() - mu * tau_s) / sigma;
    let q_up = 1.0 - phi(z);
    q_up.clamp(0.0, 1.0)
}

fn phi(z: f64) -> f64 {
    0.5 * (1.0 + erf(z / std::f64::consts::SQRT_2))
}

fn erf(x: f64) -> f64 {
    // Abramowitz and Stegun 7.1.26 approximation
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let t = 1.0 / (1.0 + 0.5 * x);
    let mut poly = 0.17087277;
    poly = poly * t - 0.82215223;
    poly = poly * t + 1.48851587;
    poly = poly * t - 1.13520398;
    poly = poly * t + 0.27886807;
    poly = poly * t - 0.18628806;
    poly = poly * t + 0.09678418;
    poly = poly * t + 0.37409196;
    poly = poly * t + 1.00002368;
    let tau = t * (-x * x - 1.26551223 + t * poly).exp();
    sign * (1.0 - tau)
}

#[cfg(test)]
mod tests {
    use super::compute_q_up;

    #[test]
    fn q_up_increases_when_st_above_s0() {
        let s0 = 100.0;
        let st = 100.2;
        let q_up = compute_q_up(s0, st, 1.0, 0.0, 1e-6);
        assert!(q_up > 0.5, "q_up should be > 0.5 when St > S0");
    }
}
