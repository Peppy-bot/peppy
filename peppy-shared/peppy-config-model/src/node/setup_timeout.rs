//! The setup budget of a node: how long the daemon waits for the node's
//! setup to end once the node answers ready.
//!
//! A node declares its budget as `execution.setup_timeout_secs`. A node that
//! declares none gets the default setup budget, [`SetupTimeout::DEFAULT`].
//! The daemon writes the budget it applies into the runtime configuration of
//! each instance, as `lifecycle.setup_timeout_secs`, so the node can read it.

use serde::{Deserialize, Deserializer, Serialize, de};
use std::time::Duration;

/// A setup budget, in whole seconds from [`SetupTimeout::MIN_SECS`] to
/// [`SetupTimeout::MAX_SECS`]. It serializes as its number of seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct SetupTimeout(u16);

impl SetupTimeout {
    /// The smallest setup budget, in seconds.
    pub const MIN_SECS: u64 = 1;
    /// The largest setup budget, in seconds. It stays below the default run
    /// idle budget of a launch (600 s), so a silent setup gets the setup
    /// timeout and not the idle timeout.
    pub const MAX_SECS: u64 = 540;
    /// The default setup budget: the budget of a node that declares none.
    pub const DEFAULT: Self = Self(15);
    /// The largest setup budget.
    pub const MAX: Self = Self(Self::MAX_SECS as u16);

    /// The setup budget of `secs` seconds, refused outside
    /// [`Self::MIN_SECS`] to [`Self::MAX_SECS`].
    pub fn from_secs(secs: u64) -> Result<Self, SetupTimeoutOutOfRange> {
        if !(Self::MIN_SECS..=Self::MAX_SECS).contains(&secs) {
            return Err(SetupTimeoutOutOfRange {
                written: secs.to_string(),
            });
        }
        Ok(Self(secs as u16))
    }

    pub const fn as_secs(self) -> u64 {
        self.0 as u64
    }

    pub const fn as_duration(self) -> Duration {
        Duration::from_secs(self.as_secs())
    }

    /// Whether this is the default setup budget. A runtime configuration
    /// leaves the default out, so the configuration of an instance with the
    /// default budget stays byte-identical to one written before the field
    /// existed.
    pub fn is_default(&self) -> bool {
        *self == Self::DEFAULT
    }

    /// Reads a written setup budget: a whole number of seconds in range.
    /// Any other value (a number out of range, a negative or fractional
    /// number, a string, a boolean, `null`) is refused with the text it was
    /// written as.
    pub(crate) fn from_written(
        written: &serde_json::Value,
    ) -> Result<Self, SetupTimeoutOutOfRange> {
        match written.as_u64() {
            Some(secs) => Self::from_secs(secs),
            None => Err(SetupTimeoutOutOfRange {
                written: written.to_string(),
            }),
        }
    }
}

impl Default for SetupTimeout {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A written setup budget that is not a whole number of seconds from
/// [`SetupTimeout::MIN_SECS`] to [`SetupTimeout::MAX_SECS`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "a setup budget is a whole number of seconds from {min} to {max}, got {written}",
    min = SetupTimeout::MIN_SECS,
    max = SetupTimeout::MAX_SECS
)]
pub struct SetupTimeoutOutOfRange {
    written: String,
}

impl SetupTimeoutOutOfRange {
    /// The refused value as it was written.
    pub fn written(&self) -> &str {
        &self.written
    }
}

/// Reads the value through `serde_json::Value`, because `serde_json5` casts a
/// negative or fractional number that is read as an unsigned one.
impl<'de> Deserialize<'de> for SetupTimeout {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let written = serde_json::Value::deserialize(deserializer)?;
        Self::from_written(&written).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(written: &str) -> Result<SetupTimeout, String> {
        serde_json5::from_str(written).map_err(|error| error.to_string())
    }

    #[test]
    fn every_whole_number_of_the_range_is_a_setup_budget() {
        for secs in [SetupTimeout::MIN_SECS, 15, 180, SetupTimeout::MAX_SECS] {
            let budget = SetupTimeout::from_secs(secs).expect("in range");
            assert_eq!(budget.as_secs(), secs);
            assert_eq!(budget.as_duration(), Duration::from_secs(secs));
            assert_eq!(parse(&secs.to_string()), Ok(budget));
        }
    }

    #[test]
    fn the_default_and_the_maximum_are_15_and_540_seconds() {
        assert_eq!(SetupTimeout::DEFAULT.as_secs(), 15);
        assert_eq!(SetupTimeout::default(), SetupTimeout::DEFAULT);
        assert!(SetupTimeout::DEFAULT.is_default());
        assert_eq!(SetupTimeout::MAX.as_secs(), 540);
        assert!(!SetupTimeout::MAX.is_default());
    }

    #[test]
    fn a_number_outside_the_range_is_refused_with_the_range() {
        for secs in [0, SetupTimeout::MAX_SECS + 1, u64::MAX] {
            let refusal = SetupTimeout::from_secs(secs).expect_err("out of range");
            assert_eq!(
                refusal.to_string(),
                format!("a setup budget is a whole number of seconds from 1 to 540, got {secs}")
            );
        }
        for secs in [0, SetupTimeout::MAX_SECS + 1, 86_400] {
            let error = parse(&secs.to_string()).expect_err("out of range");
            assert!(
                error.contains(&format!(
                    "a setup budget is a whole number of seconds from 1 to 540, got {secs}"
                )),
                "{error}"
            );
        }
    }

    #[test]
    fn a_value_that_is_not_a_whole_number_is_refused_as_written() {
        for (written, shown) in [
            ("-1", "-1"),
            ("2.5", "2.5"),
            ("15.0", "15.0"),
            ("\"15\"", "\"15\""),
            ("true", "true"),
            ("null", "null"),
        ] {
            let error = parse(written).expect_err("not a whole number");
            assert!(
                error.contains(&format!(
                    "a setup budget is a whole number of seconds from 1 to 540, got {shown}"
                )),
                "{written}: {error}"
            );
        }
    }

    #[test]
    fn a_setup_budget_serializes_as_its_seconds() {
        let budget = SetupTimeout::from_secs(180).expect("in range");
        assert_eq!(serde_json5::to_string(&budget).unwrap(), "180");
    }
}
