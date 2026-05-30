pub mod commands;
pub mod context;
mod daemon_state;
pub mod error;
pub(crate) mod terminal;

// `terminal` itself stays crate-private; only the color gate is re-exported so
// the binary crate's `logging` module shares the one stdout color decision.
pub use terminal::colors_enabled;

#[cfg(feature = "test-support")]
pub mod test_support;
