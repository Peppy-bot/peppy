#![forbid(unsafe_code)]
//! Shared latency-reporting utilities.
//!
//! These pieces were originally private to the offline latency benchmark
//! (`core-node-internal/benches/latency.rs`). They are factored out here so the
//! offline bench and the live `peppy stack benchmark` command compute and render
//! latency the same way, from a single source of truth.
//!
//! Everything is std-only and deterministic (no I/O except the baseline file and
//! sysfs reads in [`environment`]), so it is straightforward to unit-test. The
//! crate holds zero `unsafe` (enforced by `#![forbid(unsafe_code)]`).
//!
//! ## Consumer boundary
//! Each module is the unit of the public API; reach for types through their
//! module path (`baseline::StoredStats`, `stats::Summary`, ...). The consumer
//! owns the measurement loop and decides what to print; this crate owns only the
//! stats math, the table/duration formatting, the baseline file, and the
//! environment snapshot, and it returns errors rather than printing them.

pub mod baseline;
pub mod environment;
pub mod format;
pub mod stats;
