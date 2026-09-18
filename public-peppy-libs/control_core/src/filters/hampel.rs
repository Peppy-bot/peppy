//! Online Hampel filter for rejecting impulsive spikes in a control signal.

use crate::filters::FilterError;

/// The MAD-to-sigma consistency constant for normally distributed noise.
const MAD_SCALE: f64 = 1.4826;

/// Upper bound on the window, far beyond any useful Hampel span; caps the allocation and
/// the per-sample median sort.
const MAX_WINDOW_SIZE: usize = 1024;

/// An online Hampel filter applied to a scalar signal one sample at a time. Signal-agnostic
/// like the smoothers in this module; compose an array of them to filter a vector.
///
/// Complements [`LowPassFilter`](crate::filters::LowPassFilter) and
/// [`ButterworthFilter`](crate::filters::ButterworthFilter) rather than replacing them: a
/// low-pass smears an impulsive outlier (an encoder glitch, a corrupted sample) into the
/// output, while the Hampel filter replaces it outright and passes clean samples through
/// unchanged, adding no lag and no distortion to the signal it accepts.
///
/// Each sample is compared to the median of the trailing `window_size` raw samples. It is an
/// outlier, and replaced by that median, when it deviates by more than
/// `max(n_sigmas * 1.4826 * MAD, min_threshold)`, where MAD is the window's median absolute
/// deviation. The MAD term adapts the threshold to the signal's own noise; `min_threshold`
/// (in signal units) floors it so a noise-free window, whose MAD is zero, does not flag
/// every subsequent change.
///
/// Raw samples enter the window even when rejected, so an isolated spike cannot shift the
/// median, while a genuine level shift re-centers the window and passes within
/// `window_size` samples instead of being suppressed indefinitely. Motion onset from rest
/// faster than `min_threshold` per sample is indistinguishable from a spike and is clipped
/// the same way; once the window carries the motion, its MAD widens the threshold and a
/// steady ramp passes untouched. A non-finite sample is
/// an outlier by definition: it is replaced by the median and the median takes its slot in
/// the window. The first sample seeds the whole window to itself; a non-finite first sample
/// is returned unchanged and does not seed.
#[derive(Clone, Debug)]
pub struct HampelFilter {
    window_size: usize,
    n_sigmas: f64,
    min_threshold: f64,
    /// Empty until seeded, then always `window_size` long, oldest first, finite only.
    history: Vec<f64>,
    scratch: Vec<f64>,
}

impl HampelFilter {
    /// A filter judging each sample against the median of the trailing `window_size` raw
    /// samples, with the outlier threshold `max(n_sigmas * 1.4826 * MAD, min_threshold)`.
    /// Returns [`FilterError::InvalidHampel`] unless `window_size` is in `3..=1024` (a shorter
    /// window has no meaningful median, a longer one has no filtering use and only inflates
    /// the allocation and the per-sample sort) and `n_sigmas` and `min_threshold` are finite
    /// and positive (a zero floor would reject every change once the window goes
    /// noise-free).
    pub fn new(window_size: usize, n_sigmas: f64, min_threshold: f64) -> Result<Self, FilterError> {
        let valid = (3..=MAX_WINDOW_SIZE).contains(&window_size)
            && n_sigmas.is_finite()
            && n_sigmas > 0.0
            && min_threshold.is_finite()
            && min_threshold > 0.0;
        if !valid {
            return Err(FilterError::InvalidHampel);
        }
        Ok(Self {
            window_size,
            n_sigmas,
            min_threshold,
            history: Vec::with_capacity(window_size),
            scratch: Vec::with_capacity(window_size),
        })
    }

    /// Filter one sample and advance the window. Accepted samples are returned unchanged;
    /// outliers come back as the window median. The first finite sample after construction
    /// or [`reset`](Self::reset) seeds the whole window to itself and passes through.
    pub fn filter(&mut self, x: f64) -> f64 {
        if self.history.is_empty() {
            if !x.is_finite() {
                return x;
            }
            self.history.resize(self.window_size, x);
            return x;
        }

        let median = self.window_median();
        let mad = self.window_mad(median);
        let threshold = (self.n_sigmas * MAD_SCALE * mad).max(self.min_threshold);
        let is_outlier = !x.is_finite() || (x - median).abs() > threshold;

        self.history.remove(0);
        self.history.push(if x.is_finite() { x } else { median });

        if is_outlier { median } else { x }
    }

    /// Forget the window so the next [`filter`](Self::filter) call seeds afresh.
    pub fn reset(&mut self) {
        self.history.clear();
    }

    fn window_median(&mut self) -> f64 {
        self.scratch.clear();
        self.scratch.extend_from_slice(&self.history);
        median_of_unsorted(&mut self.scratch)
    }

    fn window_mad(&mut self, median: f64) -> f64 {
        self.scratch.clear();
        self.scratch
            .extend(self.history.iter().map(|v| (v - median).abs()));
        median_of_unsorted(&mut self.scratch)
    }
}

/// The median of a non-empty slice of finite values, sorting it in place.
fn median_of_unsorted(values: &mut [f64]) -> f64 {
    assert!(!values.is_empty());
    values.sort_unstable_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        values[mid]
    } else {
        // Halve before adding so a middle pair near f64::MAX cannot overflow to infinity.
        0.5 * values[mid - 1] + 0.5 * values[mid]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Window 5 with a floor of 1.0, matching the shape of the reference teleop filters.
    fn filter() -> HampelFilter {
        HampelFilter::new(5, 3.0, 1.0).unwrap()
    }

    #[test]
    fn new_rejects_degenerate_parameters() {
        for (window, n_sigmas, min_threshold) in [
            (2, 3.0, 1.0),
            (MAX_WINDOW_SIZE + 1, 3.0, 1.0),
            (usize::MAX, 3.0, 1.0),
            (5, 0.0, 1.0),
            (5, -3.0, 1.0),
            (5, f64::NAN, 1.0),
            (5, 3.0, 0.0),
            (5, 3.0, -1.0),
            (5, 3.0, f64::INFINITY),
        ] {
            assert!(
                matches!(
                    HampelFilter::new(window, n_sigmas, min_threshold),
                    Err(FilterError::InvalidHampel)
                ),
                "({window}, {n_sigmas}, {min_threshold}) should be rejected"
            );
        }
    }

    #[test]
    fn first_sample_seeds_and_passes_through() {
        let mut f = filter();
        assert_eq!(f.filter(3.0), 3.0);
    }

    #[test]
    fn slow_motion_passes_unchanged_from_rest() {
        let mut f = filter();
        // Steps well under the floor never deviate past the trailing median by 1.0.
        for i in 0..50 {
            let x = 0.3 * f64::from(i);
            assert_eq!(f.filter(x), x, "zero lag, zero distortion at sample {i}");
        }
    }

    #[test]
    fn steady_fast_motion_passes_once_the_window_carries_it() {
        let mut f = filter();
        // Steps of 2.0 dwarf the floor, but once the window holds the ramp its MAD is
        // 2.0, widening the threshold to 3 * 1.4826 * 2 = 8.9 versus a trailing-median
        // deviation of 6.0. Only the onset from rest reads as a spike.
        let outputs: Vec<f64> = (0..20).map(|i| f.filter(2.0 * f64::from(i))).collect();
        for (i, y) in outputs.iter().enumerate().skip(5) {
            assert_eq!(*y, 2.0 * f64::from(i as u32), "steady ramp is clean");
        }
    }

    #[test]
    fn isolated_spike_is_replaced_by_the_median() {
        let mut f = filter();
        for _ in 0..5 {
            f.filter(2.0);
        }
        assert_eq!(f.filter(100.0), 2.0, "the spike comes back as the median");
        assert_eq!(f.filter(2.0), 2.0, "the next clean sample passes");
    }

    #[test]
    fn spike_in_window_does_not_shift_the_median() {
        let mut f = filter();
        for _ in 0..5 {
            f.filter(2.0);
        }
        f.filter(100.0);
        // The spike sits in the window but four of five samples still vote 2.0.
        assert_eq!(f.filter(100.0), 2.0, "a repeat spike is still rejected");
    }

    #[test]
    fn persistent_level_shift_recovers_within_the_window() {
        let mut f = filter();
        for _ in 0..5 {
            f.filter(0.0);
        }
        let outputs: Vec<f64> = (0..5).map(|_| f.filter(10.0)).collect();
        assert_eq!(outputs[0], 0.0, "the first shifted sample reads as a spike");
        assert!(
            outputs.contains(&10.0),
            "the shift passes once it dominates the window: {outputs:?}"
        );
    }

    #[test]
    fn mad_widens_the_threshold_for_a_noisy_signal() {
        let mut f = filter();
        // Window [0, 4, 8, 12, 16]: median 8, MAD 4, threshold 3 * 1.4826 * 4 = 17.8.
        f.filter(0.0);
        for x in [4.0, 8.0, 12.0, 16.0] {
            f.filter(x);
        }
        assert_eq!(f.filter(25.0), 25.0, "within the widened threshold");
    }

    #[test]
    fn min_threshold_floors_a_noise_free_window() {
        let mut f = filter();
        for _ in 0..5 {
            f.filter(2.0);
        }
        // MAD is zero, so the floor of 1.0 is the whole threshold.
        assert_eq!(f.filter(2.9), 2.9, "below the floor passes");
        assert_eq!(f.filter(2.0), 2.0);
        assert_eq!(f.filter(3.5), 2.0, "above the floor is rejected");
    }

    #[test]
    fn non_finite_sample_is_replaced_once_seeded() {
        let mut f = filter();
        for _ in 0..5 {
            f.filter(2.0);
        }
        assert_eq!(f.filter(f64::NAN), 2.0);
        assert_eq!(f.filter(f64::INFINITY), 2.0);
        assert_eq!(f.filter(2.5), 2.5, "the window stays finite and functional");
    }

    #[test]
    fn non_finite_first_sample_does_not_seed() {
        let mut f = filter();
        assert!(f.filter(f64::NAN).is_nan(), "nothing to substitute yet");
        assert_eq!(f.filter(4.0), 4.0, "the first finite sample seeds instead");
        assert_eq!(f.filter(100.0), 4.0, "and the window is live");
    }

    #[test]
    fn even_window_averages_the_middle_pair() {
        let mut f = HampelFilter::new(4, 3.0, 1.0).unwrap();
        f.filter(0.0);
        for x in [2.0, 4.0, 6.0] {
            f.filter(x);
        }
        // Window [0, 2, 4, 6]: median 3, MAD 2, threshold 3 * 1.4826 * 2 = 8.9.
        assert_eq!(f.filter(50.0), 3.0);
    }

    #[test]
    fn even_window_median_stays_finite_at_f64_max() {
        let mut f = HampelFilter::new(4, 3.0, 1.0).unwrap();
        f.filter(f64::MAX);
        // The middle pair is (MAX, MAX); a naive average would overflow to infinity and
        // a rejected sample would then plant infinity in the window.
        assert_eq!(f.filter(f64::NAN), f64::MAX);
        assert!(f.filter(f64::NAN).is_finite(), "the window stays finite");
    }

    #[test]
    fn reset_reseeds_on_the_next_sample() {
        let mut f = filter();
        for _ in 0..5 {
            f.filter(2.0);
        }
        f.reset();
        assert_eq!(
            f.filter(9.0),
            9.0,
            "after reset the next sample seeds again"
        );
    }
}
