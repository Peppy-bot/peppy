//! Shared latency-reporting utilities.
//!
//! These pieces were originally private to the offline latency benchmark
//! (`core-node-internal/benches/latency.rs`). They are factored out here so the
//! offline bench and the live `peppy stack benchmark` command compute and render
//! latency the same way, from a single source of truth.
//!
//! Everything is std-only and deterministic (no I/O except the baseline file and
//! sysfs reads in [`environment`]), so it is straightforward to unit-test.

pub mod baseline;
pub mod environment;
pub mod format;
pub mod stats;

pub use baseline::StoredStats;
pub use stats::Summary;
