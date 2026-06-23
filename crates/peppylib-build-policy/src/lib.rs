//! Build-time decision logic specific to the `peppyos` workspace.
//!
//! This crate holds the rebuild policy for peppylib's embedded native
//! extensions. It is consumed only by `generator`'s build script and is kept
//! separate from the shared `build-helpers` crate (which lives in
//! `nodes_shared_code/peppyos-shared`) so that no peppyos-specific code is
//! pulled into the shared build-helper dependency.

#![forbid(unsafe_code)]

mod so_build;

pub use so_build::{BuildProfile, should_build_host, should_cross_compile};
