//! Second-order Butterworth low-pass filter for smoothing a control signal.

use crate::filters::FilterError;

/// A second-order (two-pole) Butterworth low-pass filter applied to a scalar signal one
/// sample at a time. Signal-agnostic like [`LowPassFilter`](crate::filters::LowPassFilter); compose
/// an array of them to filter a vector.
///
/// Preferred over the one-pole [`LowPassFilter`](crate::filters::LowPassFilter) when the goal is to
/// strip high-frequency content (per-tick jerk in a joint command) while disturbing the
/// low-frequency trajectory as little as possible: the Butterworth response is maximally
/// flat in the passband and rolls off at -40 dB/decade (twice the one-pole's -20), so for a
/// given cutoff it removes more of the roughness with less passband droop.
///
/// The analog prototype `H(s) = wc^2 / (s^2 + sqrt(2) wc s + wc^2)` is mapped to the
/// biquad `y[n] = b0 x[n] + b1 x[n-1] + b2 x[n-2] - a1 y[n-1] - a2 y[n-2]` by the bilinear
/// transform with frequency pre-warping (`K = tan(pi fc Ts)`), so the -3 dB point lands
/// exactly at `fc`. DC gain is unity, so a settled constant passes through untouched. The
/// first sample seeds the entire history to itself, so the filter starts settled on its
/// input rather than ringing up from zero.
#[derive(Clone, Copy, Debug)]
pub struct ButterworthFilter {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    state: Option<Biquad>,
}

/// The two-sample input/output history a biquad carries between samples.
#[derive(Clone, Copy, Debug)]
struct Biquad {
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl ButterworthFilter {
    /// A filter with cutoff `cutoff_hz` sampled every `sample_period_s`, or
    /// [`FilterError::InvalidButterworth`] if either is not finite and positive, or the cutoff is not
    /// below the Nyquist frequency (`0.5 / sample_period_s`): at or above Nyquist the
    /// pre-warp `tan(pi fc Ts)` is undefined, so no valid filter exists.
    pub fn from_cutoff(cutoff_hz: f64, sample_period_s: f64) -> Result<Self, FilterError> {
        let valid = cutoff_hz.is_finite()
            && cutoff_hz > 0.0
            && sample_period_s.is_finite()
            && sample_period_s > 0.0
            && cutoff_hz < 0.5 / sample_period_s;
        if !valid {
            return Err(FilterError::InvalidButterworth);
        }
        // Pre-warped cutoff and the Butterworth quality factor Q = 1/sqrt(2).
        let k = (std::f64::consts::PI * cutoff_hz * sample_period_s).tan();
        let k2 = k * k;
        let norm = 1.0 / (1.0 + std::f64::consts::SQRT_2 * k + k2);
        Ok(Self {
            b0: k2 * norm,
            b1: 2.0 * k2 * norm,
            b2: k2 * norm,
            a1: 2.0 * (k2 - 1.0) * norm,
            a2: (1.0 - std::f64::consts::SQRT_2 * k + k2) * norm,
            state: None,
        })
    }

    /// Filter one sample and advance the state. The first sample after construction or
    /// [`reset`](Self::reset) seeds the whole history to itself and is passed through
    /// unchanged, so there is no startup transient from an assumed-zero history.
    pub fn filter(&mut self, x: f64) -> f64 {
        match &mut self.state {
            None => {
                self.state = Some(Biquad {
                    x1: x,
                    x2: x,
                    y1: x,
                    y2: x,
                });
                x
            }
            Some(s) => {
                let y =
                    self.b0 * x + self.b1 * s.x1 + self.b2 * s.x2 - self.a1 * s.y1 - self.a2 * s.y2;
                s.x2 = s.x1;
                s.x1 = x;
                s.y2 = s.y1;
                s.y1 = y;
                y
            }
        }
    }

    /// Forget the filter state so the next [`filter`](Self::filter) call seeds afresh.
    pub fn reset(&mut self) {
        self.state = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::{FRAC_1_SQRT_2, TAU};

    const TS: f64 = 0.001; // 1 kHz sampling: room for a cutoff well below Nyquist.

    #[test]
    fn from_cutoff_rejects_non_positive_non_finite_or_above_nyquist() {
        assert!(matches!(
            ButterworthFilter::from_cutoff(0.0, TS),
            Err(FilterError::InvalidButterworth)
        ));
        assert!(matches!(
            ButterworthFilter::from_cutoff(-1.0, TS),
            Err(FilterError::InvalidButterworth)
        ));
        assert!(matches!(
            ButterworthFilter::from_cutoff(10.0, 0.0),
            Err(FilterError::InvalidButterworth)
        ));
        assert!(matches!(
            ButterworthFilter::from_cutoff(f64::NAN, TS),
            Err(FilterError::InvalidButterworth)
        ));
        // At/above Nyquist (500 Hz here) there is no valid filter.
        assert!(matches!(
            ButterworthFilter::from_cutoff(500.0, TS),
            Err(FilterError::InvalidButterworth)
        ));
        assert!(matches!(
            ButterworthFilter::from_cutoff(600.0, TS),
            Err(FilterError::InvalidButterworth)
        ));
    }

    #[test]
    fn first_sample_seeds_the_state() {
        let mut f = ButterworthFilter::from_cutoff(50.0, TS).unwrap();
        assert_eq!(
            f.filter(3.0),
            3.0,
            "the first sample passes through, no transient from zero"
        );
    }

    #[test]
    fn constant_input_settles_to_that_constant() {
        let mut f = ButterworthFilter::from_cutoff(20.0, TS).unwrap();
        f.filter(0.0); // seed low
        for _ in 0..2000 {
            f.filter(5.0);
        }
        assert!((f.filter(5.0) - 5.0).abs() < 1e-9, "DC gain is unity");
    }

    // The defining property: at the cutoff frequency the response is exactly -3 dB
    // (amplitude 1/sqrt(2)). This pins the coefficients as a real Butterworth tuned to fc.
    #[test]
    fn minus_three_db_at_the_cutoff() {
        let fc = 10.0;
        let mut f = ButterworthFilter::from_cutoff(fc, TS).unwrap();
        let n = 40_000;
        let mut peak = 0.0_f64;
        for k in 0..n {
            let x = (TAU * fc * k as f64 * TS).sin();
            let y = f.filter(x);
            if k > n / 2 {
                peak = peak.max(y.abs()); // steady-state amplitude only
            }
        }
        assert!(
            (peak - FRAC_1_SQRT_2).abs() < 0.01,
            "amplitude at cutoff should be 1/sqrt(2) (-3 dB), got {peak:.4}"
        );
    }

    // Two poles roll off faster than one: an octave above cutoff the Butterworth attenuates
    // a tone more than the first-order low-pass at the same cutoff.
    #[test]
    fn rolls_off_steeper_than_first_order() {
        use crate::filters::LowPassFilter;
        let fc = 10.0;
        let f_tone = 40.0; // two octaves above cutoff
        let amp = |bw: bool| {
            let n = 40_000;
            let mut lp = LowPassFilter::from_cutoff(fc, TS).unwrap();
            let mut bwf = ButterworthFilter::from_cutoff(fc, TS).unwrap();
            let mut peak = 0.0_f64;
            for k in 0..n {
                let x = (TAU * f_tone * k as f64 * TS).sin();
                let y = if bw { bwf.filter(x) } else { lp.filter(x) };
                if k > n / 2 {
                    peak = peak.max(y.abs());
                }
            }
            peak
        };
        assert!(
            amp(true) < amp(false),
            "Butterworth attenuates the high tone more"
        );
    }

    #[test]
    fn reset_reseeds_on_the_next_sample() {
        let mut f = ButterworthFilter::from_cutoff(20.0, TS).unwrap();
        f.filter(0.0);
        f.filter(1.0);
        f.reset();
        assert_eq!(
            f.filter(9.0),
            9.0,
            "after reset the next sample seeds again"
        );
    }
}
