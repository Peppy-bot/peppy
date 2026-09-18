//! A floating-point magnitude that has already proven itself usable.

use thiserror::Error;

/// A number that cannot bound anything.
#[derive(Debug, Clone, Copy, Error)]
#[error("must be a positive finite number, got {0}")]
pub struct NotPositiveFinite(pub f64);

/// A positive, finite `f64`: holding one is the proof.
///
/// NaN and the infinities cannot be inside it, so a clamp or a divisor built
/// against one cannot be silently disabled, and a value that was never
/// checked cannot be passed where a `PositiveFinite` is expected. The name
/// states the invariant and nothing more; whether it is a ceiling, a floor or
/// a rate is the caller's meaning to give.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct PositiveFinite(f64);

impl PositiveFinite {
    /// The only way to obtain one.
    pub fn parse(value: f64) -> Result<Self, NotPositiveFinite> {
        (value.is_finite() && value > 0.0)
            .then_some(Self(value))
            .ok_or(NotPositiveFinite(value))
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

impl std::fmt::Display for PositiveFinite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_that_is_not_positive_and_finite_is_refused() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -0.0, -1.0] {
            assert!(
                PositiveFinite::parse(value).is_err(),
                "{value} must be refused"
            );
        }
    }

    #[test]
    fn a_parsed_value_is_held_unchanged() {
        assert_eq!(PositiveFinite::parse(0.25).unwrap().get(), 0.25);
        assert_eq!(
            PositiveFinite::parse(f64::MIN_POSITIVE).unwrap().get(),
            f64::MIN_POSITIVE
        );
    }
}
