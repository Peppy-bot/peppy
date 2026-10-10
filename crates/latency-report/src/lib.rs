#![forbid(unsafe_code)]
//! Shared latency-reporting utilities.
//!
//! The offline latency bench (`core-node-internal/benches/latency.rs`), the
//! daemon's stack benchmark service and the `peppy stack benchmark` command
//! compute and render latency with these pieces, so they agree.
//!
//! Everything is std-only. The only I/O is the baseline file, the machine id
//! that names it, and the sysfs and procfs reads in [`environment`]; the rest
//! is deterministic, so it is straightforward to unit-test. The crate holds
//! zero `unsafe` (enforced by `#![forbid(unsafe_code)]`).
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
