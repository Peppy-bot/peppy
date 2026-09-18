//! The minimum-jerk profile: the shape a bounded motion follows between two
//! endpoints, and the duration that shape needs to stay inside a velocity
//! budget. Nothing here knows about arms, joints, or Cartesian space: the
//! blend parameter is dimensionless and the caller supplies the ratio.

use thiserror::Error;

/// A budget that cannot size a duration.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("velocity ratio and requested duration must be finite and non-negative")]
pub struct InvalidVelocityBudget;

/// Peak normalised velocity of the quintic blend `s(tau)`: `ds/dtau` at
/// `tau = 0.5`. Over a blend of duration `T`, a quantity changing by `delta`
/// peaks at `QUINTIC_PEAK_VELOCITY * delta / T`, which is what turns a
/// velocity budget into a minimum duration (see
/// [`velocity_limited_duration`]). 15/8 exactly.
pub const QUINTIC_PEAK_VELOCITY: f64 = 1.875;

/// The quintic minimum-jerk blend `s(tau)` and its derivative `ds/dtau`, for
/// `tau = t/T` in `[0, 1]`. `s` runs 0 to 1 with `s'(0) = s'(1) = 0` and
/// `s''(0) = s''(1) = 0`, so a path blended by it starts and stops with zero
/// velocity and zero acceleration: the smoothest profile that meets fixed
/// boundary conditions.
///
/// `tau` outside `[0, 1]` extrapolates the polynomial; callers that hold at
/// the endpoints clamp before calling.
pub fn quintic(tau: f64) -> (f64, f64) {
    let s = ((6.0 * tau - 15.0) * tau + 10.0) * tau * tau * tau;
    let ds_dtau = ((30.0 * tau - 60.0) * tau + 30.0) * tau * tau;
    (s, ds_dtau)
}

/// Smallest duration (s) that keeps a quintic-blended motion within its
/// velocity limits, floored at `requested_secs` so a caller can ask for a
/// slower motion. `peak_velocity_ratio` is the largest per-component ratio of
/// travel to that component's limit (units cancel, so joint radians and
/// Cartesian metres size the same way); the quintic's peak factor scales it to
/// the minimum feasible duration.
///
/// Both arguments must be finite and non-negative. They are checked rather
/// than assumed because `f64::max` discards a NaN operand instead of
/// propagating it, so an unchecked NaN would leave here as a plausible
/// duration, and two negatives would leave as a negative one, which inverts
/// the bound it is supposed to impose.
pub fn velocity_limited_duration(
    peak_velocity_ratio: f64,
    requested_secs: f64,
) -> Result<f64, InvalidVelocityBudget> {
    let sane = |v: f64| v.is_finite() && v >= 0.0;
    if !sane(peak_velocity_ratio) || !sane(requested_secs) {
        return Err(InvalidVelocityBudget);
    }
    Ok(requested_secs.max(QUINTIC_PEAK_VELOCITY * peak_velocity_ratio))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_blend_runs_zero_to_one_and_rests_at_both_ends() {
        let (s0, ds0) = quintic(0.0);
        let (s1, ds1) = quintic(1.0);
        assert_eq!(s0, 0.0);
        assert_eq!(ds0, 0.0);
        assert!((s1 - 1.0).abs() < 1e-12);
        assert!(ds1.abs() < 1e-12);
    }

    #[test]
    fn the_blend_is_monotone_and_symmetric() {
        let mut prev = 0.0;
        for i in 0..=100 {
            let tau = f64::from(i) / 100.0;
            let (s, ds) = quintic(tau);
            assert!(s >= prev - 1e-12, "s decreased at tau={tau}");
            assert!(ds >= -1e-12, "negative rate at tau={tau}");
            prev = s;
            // s(tau) + s(1 - tau) == 1 for a symmetric blend.
            let (mirror, _) = quintic(1.0 - tau);
            assert!((s + mirror - 1.0).abs() < 1e-12, "asymmetric at tau={tau}");
        }
    }

    #[test]
    fn the_peak_rate_is_the_documented_factor() {
        let (_, ds_mid) = quintic(0.5);
        assert!((ds_mid - QUINTIC_PEAK_VELOCITY).abs() < 1e-12);
        // And it really is the maximum over the blend.
        for i in 0..=1000 {
            let (_, ds) = quintic(f64::from(i) / 1000.0);
            assert!(ds <= QUINTIC_PEAK_VELOCITY + 1e-12);
        }
    }

    #[test]
    fn a_sized_duration_holds_the_peak_inside_the_budget() {
        // One unit of travel against a 0.5 unit/s limit: ratio 2 s.
        let duration = velocity_limited_duration(2.0, 0.0).unwrap();
        let peak = QUINTIC_PEAK_VELOCITY * 1.0 / duration;
        assert!(peak <= 0.5 + 1e-12, "peak {peak} exceeded the limit");
    }

    #[test]
    fn a_slower_request_wins() {
        assert_eq!(velocity_limited_duration(2.0, 30.0).unwrap(), 30.0);
    }

    #[test]
    fn a_zero_motion_takes_whatever_was_asked_for() {
        assert_eq!(velocity_limited_duration(0.0, 0.0).unwrap(), 0.0);
        assert_eq!(velocity_limited_duration(0.0, 4.0).unwrap(), 4.0);
    }

    #[test]
    fn unusable_inputs_are_refused_rather_than_absorbed() {
        // f64::max drops a NaN operand, so an unchecked NaN would leave here
        // as a plausible duration; two negatives would leave as a negative one.
        for (ratio, requested) in [
            (f64::NAN, 2.0),
            (2.0, f64::NAN),
            (f64::INFINITY, 1.0),
            (1.0, f64::INFINITY),
            (f64::NEG_INFINITY, 1.0),
            (-1.0, -2.0),
            (-3.0, 0.0),
            (1.0, -1.0),
        ] {
            assert!(
                velocity_limited_duration(ratio, requested).is_err(),
                "accepted ratio={ratio} requested={requested}"
            );
        }
    }
}
