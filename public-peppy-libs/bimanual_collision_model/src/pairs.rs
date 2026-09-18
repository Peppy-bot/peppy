//! Which body pairs the runtime checks, by name, for explicit pair lists
//! (tests and special-purpose tools).
//!
//! Pairs are data, not logic: the model checks whatever list it is given.
//! Derived models get their pair set from the URDF at construction instead.

/// One checked pair of bodies, by URDF link name.
#[derive(Debug, Clone, PartialEq)]
pub struct PairSpec {
    pub a: String,
    pub b: String,
}

impl PairSpec {
    pub fn new(a: impl Into<String>, b: impl Into<String>) -> Self {
        Self {
            a: a.into(),
            b: b.into(),
        }
    }
}
