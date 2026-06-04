//! Build-script helpers shared across peppy crates.
//!
//! Functionality is grouped into focused submodules and re-exported flat so
//! build scripts can keep calling `build_helpers::<fn>`.

mod cargo;
mod command;
mod download;
mod fs;
mod hash;

pub use cargo::{build_target_triple, cargo_install_binary, embed_git_tag, find_bundled_capnp};
pub use command::{CommandOutput, run_command, run_command_streaming};
pub use download::{download_file, extract_zip_entry};
pub use fs::{acquire_file_lock, cache_dir, copy_if_changed, set_executable, write_if_changed};
pub use hash::verify_sha256;
