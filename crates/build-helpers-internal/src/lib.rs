//! peppyOS-local build-script helpers.
//!
//! Helpers shared with the public-facing crates in `nodes_shared_code` live in
//! the `build-helpers-shared` crate; this crate holds the build tooling used
//! only inside peppyOS. Functions are grouped into focused submodules and
//! re-exported flat so build scripts call `build_helpers::<fn>`.

#![forbid(unsafe_code)]

mod command;
mod env;
mod fs;
mod hash;
mod so_build;

pub use command::{CommandOutput, run_command, run_command_streaming, run_command_with_timeout};
pub use env::build_target_triple;
pub use fs::write_if_changed;
pub use hash::verify_sha256;
pub use so_build::{BuildProfile, should_build_host, should_cross_compile};
