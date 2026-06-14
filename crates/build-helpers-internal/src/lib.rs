//! Build-script helpers shared across peppy crates.
//!
//! Functionality is grouped into focused submodules and re-exported flat so
//! build scripts can keep calling `build_helpers::<fn>`.

#![forbid(unsafe_code)]

mod cargo;
mod command;
mod fs;
mod hash;
mod so_build;

pub use cargo::{build_target_triple, cargo_install_binary, embed_git_tag, find_bundled_capnp};
pub use command::{CommandOutput, run_command, run_command_streaming, run_command_with_timeout};
pub use fs::{acquire_file_lock, cache_dir, copy_if_changed, set_executable, write_if_changed};
pub use hash::verify_sha256;
pub use so_build::{BuildProfile, should_build_host, should_cross_compile};
