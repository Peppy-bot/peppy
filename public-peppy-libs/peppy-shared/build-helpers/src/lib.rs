//! Generic build-script helpers shared across peppy crates.
//!
//! This crate lives in `public-peppy-libs/peppy-shared` so it sits at the
//! bottom of the dependency graph: both the `peppy` workspace crates and the
//! `peppy-shared` crates depend on it, never the other way around. Helpers
//! that are specific to a single `peppy` crate (for example the peppylib
//! native-extension rebuild policy) live in their own crate next to that
//! consumer instead, so this crate stays free of reverse paths into `peppy`.
//!
//! Functionality is grouped into focused submodules and re-exported flat so
//! build scripts can keep calling `build_helpers::<fn>`.

#![forbid(unsafe_code)]

mod cargo;
mod command;
mod fs;
mod hash;

pub use cargo::{
    UnsupportedCapnpTarget, build_target_triple, bundled_capnp_for_embedding, cargo_install_binary,
    embed_git_tag, host_capnp_for_execution, peppy_shared_dir,
};
pub use command::{CommandOutput, run_command, run_command_streaming, run_command_with_timeout};
pub use fs::{acquire_file_lock, cache_dir, copy_if_changed, set_executable, write_if_changed};
pub use hash::verify_sha256;
