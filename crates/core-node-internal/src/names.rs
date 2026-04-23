//! Service and action name constants, re-exported from `core-node-api`.
//!
//! These live in [`core_node_api::names`] so that both the server side
//! (this crate) and non-peppylib clients can share the same wire identifiers
//! without creating a dependency cycle through peppylib.

pub use core_node_api::names::*;
