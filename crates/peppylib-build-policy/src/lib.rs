//! Build-time logic specific to the `peppy` workspace.
//!
//! This crate holds the rebuild policy for peppylib's embedded native
//! extensions, and reads an extension out of the wheel that maturin packs it
//! into. A build script cannot host its own `#[test]`, so this logic lives
//! here, where the CI compiles and tests it on every host. It is consumed only
//! by `generator`'s build script and is kept separate from the shared
//! `build-helpers` crate (which lives in `peppy-shared`) so that no
//! peppy-specific code is pulled into the shared build-helper dependency.

#![forbid(unsafe_code)]

mod so_build;
mod wheel;

pub use so_build::{
    BuildProfile, UnusableSo, cross_build_enabled, cross_build_requested, should_build_host,
    should_cross_compile, should_embed_so, unusable_so,
};
pub use wheel::{WheelError, native_extension_of_wheel};
