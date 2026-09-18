//! First-order low-pass filter for smoothing a noisy control signal.

use crate::filters::FilterError;

/// A first-order (single-pole) low-pass filter applied to a scalar signal one sample at a
/// time. Signal-agnostic: point it at a velocity, an opening fraction, or any channel that
/// carries sample noise; compose an array of them to filter a vector.
///
/// The recurrence is `y = alpha*y_prev + (1 - alpha)*x`, with
/// `alpha = 1 / (1 + Ts*2*pi*fc)` for cutoff `fc` (Hz) and sample period `Ts` (s): the
/// one-pole RC discretization. Higher `fc` means less smoothing (`alpha -> 0`, the output
/// tracks the input); lower `fc` means more (`alpha -> 1`, the output lags). The first
/// sample seeds the state, so the filter starts on its input rather than ringing up from
/// zero.
#[derive(Clone, Copy, Debug)]
pub struct LowPassFilter {
    alpha: f64,
    state: Option<f64>,
}

impl LowPassFilter {
    /// A filter with cutoff `cutoff_hz` sampled every `sample_period_s`, or
    /// [`FilterError::InvalidLowPass`] if either is not finite and positive: a non-positive cutoff
    /// or period has no physical meaning and would produce a degenerate `alpha`.
    pub fn from_cutoff(cutoff_hz: f64, sample_period_s: f64) -> Result<Self, FilterError> {
        let valid = cutoff_hz.is_finite()
            && cutoff_hz > 0.0
            && sample_period_s.is_finite()
            && sample_period_s > 0.0;
        if !valid {
            return Err(FilterError::InvalidLowPass);
        }
        let alpha = 1.0 / (1.0 + sample_period_s * std::f64::consts::TAU * cutoff_hz);
        Ok(Self { alpha, state: None })
    }

    /// Filter one sample and advance the state. The first sample after construction or
    /// [`reset`](Self::reset) is passed through unchanged to seed the state, so there is no
    /// startup transient from an assumed-zero history.
    pub fn filter(&mut self, x: f64) -> f64 {
        let y = match self.state {
            Some(prev) => self.alpha * prev + (1.0 - self.alpha) * x,
            None => x,
        };
        self.state = Some(y);
        y
    }

    /// Forget the filter state so the next [`filter`](Self::filter) call seeds afresh.
    pub fn reset(&mut self) {
        self.state = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ts and fc chosen so alpha is exactly 0.5: Ts*2*pi*fc = 1.
    const TS: f64 = 0.01;
    fn half_alpha_cutoff() -> f64 {
        1.0 / (std::f64::consts::TAU * TS)
    }

    #[test]
    fn from_cutoff_rejects_non_positive_or_non_finite() {
        assert!(matches!(
            LowPassFilter::from_cutoff(0.0, TS),
            Err(FilterError::InvalidLowPass)
        ));
        assert!(matches!(
            LowPassFilter::from_cutoff(-1.0, TS),
            Err(FilterError::InvalidLowPass)
        ));
        assert!(matches!(
            LowPassFilter::from_cutoff(90.0, 0.0),
            Err(FilterError::InvalidLowPass)
        ));
        assert!(matches!(
            LowPassFilter::from_cutoff(f64::NAN, TS),
            Err(FilterError::InvalidLowPass)
        ));
        assert!(matches!(
            LowPassFilter::from_cutoff(90.0, f64::INFINITY),
            Err(FilterError::InvalidLowPass)
        ));
    }

    #[test]
    fn first_sample_seeds_the_state() {
        let mut f = LowPassFilter::from_cutoff(90.0, TS).unwrap();
        assert_eq!(
            f.filter(3.0),
            3.0,
            "the first sample passes through, no transient from zero"
        );
    }

    #[test]
    fn recurrence_matches_alpha() {
        let mut f = LowPassFilter::from_cutoff(half_alpha_cutoff(), TS).unwrap();
        assert_eq!(f.filter(0.0), 0.0); // seed
        assert_eq!(f.filter(1.0), 0.5); // 0.5*0 + 0.5*1
        assert_eq!(f.filter(1.0), 0.75); // 0.5*0.5 + 0.5*1
    }

    #[test]
    fn constant_input_settles_to_that_constant() {
        let mut f = LowPassFilter::from_cutoff(20.0, TS).unwrap();
        f.filter(0.0); // seed low
        for _ in 0..1000 {
            f.filter(5.0);
        }
        assert!((f.filter(5.0) - 5.0).abs() < 1e-9, "DC gain is unity");
    }

    #[test]
    fn higher_cutoff_smooths_less() {
        let step = |cutoff: f64| {
            let mut f = LowPassFilter::from_cutoff(cutoff, TS).unwrap();
            f.filter(0.0); // seed
            f.filter(1.0) // one step response
        };
        // A higher cutoff tracks the step more closely (less lag) than a lower one.
        assert!(step(200.0) > step(20.0));
    }

    #[test]
    fn reset_reseeds_on_the_next_sample() {
        let mut f = LowPassFilter::from_cutoff(20.0, TS).unwrap();
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
