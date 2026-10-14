//! Monotone piecewise-linear fan curve (temperature C -> PWM percent).
//! Default curve is the natural PWM formulation of the project's cooling
//! philosophy; the directive's 50/40 example ships as the documented
//! "aggressive" preset in the README.

/// Default curve points: [(temp_c, pwm_percent)].
pub const DEFAULT_FAN_CURVE: &[(f32, u32)] = &[(40.0, 0), (50.0, 30), (70.0, 60), (82.0, 100)];

/// Aggressive preset (directive example): engage early, back off late.
pub const AGGRESSIVE_FAN_CURVE: &[(f32, u32)] = &[(40.0, 0), (50.0, 40), (60.0, 70), (70.0, 100)];

/// Interpolate the curve at `temp_c`, clamped to 0..100. Non-monotone or
/// short curves are sanitized (sorted by temp, deduped, min 2 points).
pub fn interp(curve: &[(f32, u32)], temp_c: f32) -> u32 {
    if curve.len() < 2 {
        return fallback(temp_c);
    }
    let mut pts: Vec<(f32, u32)> = curve.to_vec();
    pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    pts.dedup_by(|a, b| a.0 == b.0);
    if pts.len() < 2 {
        return fallback(temp_c);
    }
    if temp_c <= pts[0].0 {
        return clamp_pct(pts[0].1);
    }
    if temp_c >= pts[pts.len() - 1].0 {
        return clamp_pct(pts[pts.len() - 1].1);
    }
    for w in pts.windows(2) {
        let (t0, p0) = (w[0].0, w[0].1);
        let (t1, p1) = (w[1].0, w[1].1);
        if temp_c >= t0 && temp_c <= t1 {
            let frac = (temp_c - t0) / (t1 - t0);
            let pct = p0 as f32 + frac * (p1 as f32 - p0 as f32);
            return clamp_pct(pct.round() as u32);
        }
    }
    fallback(temp_c)
}

fn clamp_pct(v: u32) -> u32 {
    v.min(100)
}

fn fallback(temp_c: f32) -> u32 {
    interp(DEFAULT_FAN_CURVE, temp_c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_curve_endpoints_and_mid() {
        assert_eq!(interp(DEFAULT_FAN_CURVE, 30.0), 0);
        assert_eq!(interp(DEFAULT_FAN_CURVE, 40.0), 0);
        assert_eq!(interp(DEFAULT_FAN_CURVE, 82.0), 100);
        assert_eq!(interp(DEFAULT_FAN_CURVE, 95.0), 100);
        assert_eq!(interp(DEFAULT_FAN_CURVE, 60.0), 45); // midpoint of 30..60
    }

    #[test]
    fn aggressive_preset_hits_100_at_70() {
        assert_eq!(interp(AGGRESSIVE_FAN_CURVE, 50.0), 40);
        assert_eq!(interp(AGGRESSIVE_FAN_CURVE, 70.0), 100);
        assert_eq!(interp(AGGRESSIVE_FAN_CURVE, 45.0), 20);
    }

    #[test]
    fn garbage_curves_never_panic() {
        assert_eq!(interp(&[], 50.0), interp(DEFAULT_FAN_CURVE, 50.0));
        assert_eq!(interp(&[(50.0, 50)], 60.0), interp(DEFAULT_FAN_CURVE, 60.0));
        // unsorted input is sanitized
        assert_eq!(
            interp(&[(70.0, 60), (40.0, 0), (50.0, 30), (82.0, 100)], 55.0),
            interp(DEFAULT_FAN_CURVE, 55.0)
        );
    }
}
