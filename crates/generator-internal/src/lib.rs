#![forbid(unsafe_code)]
//! Generates a node's `peppygen` interface library (Rust or Python) from its `peppy.json5`.
//!
//! # Entry point
//! [`generate_peppygen_lib`] is the single function consumers call. It reads the node config,
//! collects exposed + consumed interfaces, runs the selected [`LanguageGenerator`] backend, and
//! writes the library to `<node_dir>/.peppy/libs/peppygen` (vendoring peppylib + deps). See its
//! own docs for the full filesystem-effect surface.
//!
//! # Backends
//! [`RustGenerator`] and [`PythonGenerator`] implement [`LanguageGenerator`]. Construct via
//! `new()` / `Default`, optionally configure with `set_parameters` (both) / `set_container`
//! (Python) in any order, register interfaces, then `build()` (which consumes the generator).
//! Most callers never touch the backends directly; only the latency test harness does.
//!
//! # Value objects the caller builds
//! [`DeploymentInterface`] wraps an [`InterfaceVariant`]; consumed variants carry a
//! [`DependencyContext`] (`native` / `conformed` / `interface`, plus `with_link_id` taking a
//! [`WireLinkId`]). [`InterfaceOrigin`] tags artifacts pulled in via `conforms_to`.
//! [`ConsumedActionMessage`] bundles an action's goal/feedback/result formats. These types form
//! the deliberate, semver-relevant assembly contract between this crate and its consumer: their
//! public fields are the construction protocol, so changes to them are breaking changes.
//!
//! # Misc
//! [`CrateDeployMode`] (`Symlink` default / `Copy` for containers) controls vendoring.
//! [`GeneratorError`] is the error type (re-exported for `#[from]`).
//!
//! The internal `generator` module is intentionally **private**, so any `pub` inside it stays
//! crate-internal; the crate's true external surface is exactly the re-exports below.

mod error;
mod generator;

pub use error::Error as GeneratorError;

pub use generator::common::CrateDeployMode;
pub use generator::generate_peppygen_lib;
pub use generator::python::PythonGenerator;
pub use generator::rust::RustGenerator;
pub use generator::types::{
    ConsumedActionMessage, DependencyContext, DeploymentInterface, InterfaceOrigin,
    InterfaceVariant, LanguageGenerator, PeerContext, WireLinkId,
};
