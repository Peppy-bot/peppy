//! Percentile and summary math over a set of nanosecond samples.

/// Aggregated latency statistics for one set of samples. All fields are in
/// nanoseconds except `count`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub p50_ns: u64,
    pub p90_ns: u64,
    pub mean_ns: u64,
    pub count: u64,
}

/// Nearest-rank percentile over an already-sorted ascending slice.
///
/// `p` is a fraction in `[0.0, 1.0]` (e.g. `0.5` for the median). Returns `0`
/// for an empty slice. Internal to [`summarize`]; consumers read percentiles off
/// the [`Summary`] it returns.
fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let clamped = p.clamp(0.0, 1.0);
    let idx = (((sorted.len() - 1) as f64) * clamped).round() as usize;
    sorted[idx]
}

/// Summarize raw nanosecond samples into p50 / p90 / mean / count.
///
/// The input is not required to be sorted — it is copied and sorted internally.
/// An empty input yields an all-zero [`Summary`].
pub fn summarize(samples: &[u64]) -> Summary {
    if samples.is_empty() {
        return Summary::default();
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let count = sorted.len() as u64;
    let sum: u128 = sorted.iter().map(|&v| v as u128).sum();
    let mean_ns = (sum / count as u128) as u64;
    Summary {
        p50_ns: percentile(&sorted, 0.5),
        p90_ns: percentile(&sorted, 0.9),
        mean_ns,
        count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_empty_is_zero() {
        assert_eq!(percentile(&[], 0.5), 0);
    }

    #[test]
    fn percentile_nearest_rank() {
        let sorted = [10, 20, 30, 40, 50];
        assert_eq!(percentile(&sorted, 0.0), 10);
        assert_eq!(percentile(&sorted, 0.5), 30);
        assert_eq!(percentile(&sorted, 1.0), 50);
    }

    #[test]
    fn percentile_clamps_out_of_range() {
        let sorted = [1, 2, 3];
        assert_eq!(percentile(&sorted, -1.0), 1);
        assert_eq!(percentile(&sorted, 2.0), 3);
    }

    #[test]
    fn percentile_rounds_fractional_rank_to_nearest() {
        // 9 elements, indices 0..=8. The nearest-rank index is rounded:
        // 0.9 -> (8 * 0.9) = 7.2 -> 7, and 0.95 -> 7.6 -> 8.
        let sorted: Vec<u64> = (0..9).collect();
        assert_eq!(percentile(&sorted, 0.9), 7);
        assert_eq!(percentile(&sorted, 0.95), 8);
    }

    #[test]
    fn summarize_empty_is_default() {
        assert_eq!(summarize(&[]), Summary::default());
    }

    #[test]
    fn summarize_unsorted_input() {
        let s = summarize(&[50, 10, 30, 20, 40]);
        assert_eq!(s.count, 5);
        assert_eq!(s.p50_ns, 30);
        assert_eq!(s.p90_ns, 50);
        assert_eq!(s.mean_ns, 30);
    }

    #[test]
    fn summarize_single_sample() {
        let s = summarize(&[7]);
        assert_eq!(
            s,
            Summary {
                p50_ns: 7,
                p90_ns: 7,
                mean_ns: 7,
                count: 1
            }
        );
    }
}
