#![allow(dead_code)] // Each test binary uses only a subset of these shared helpers.

mod actions;
mod daemon;
mod fixtures;
mod poll;
mod services;

// Flat re-exports so existing `common::foo()` call sites keep working; each
// test binary uses a different subset of the groups.
#[allow(unused_imports)]
pub use actions::*;
#[allow(unused_imports)]
pub use daemon::*;
#[allow(unused_imports)]
pub use fixtures::*;
#[allow(unused_imports)]
pub use poll::*;
#[allow(unused_imports)]
pub use services::*;

use core_node::names;
use peppylib::messaging::SenderTarget;

/// Default tag used by tests when building a [`SenderTarget`]. Matches the
/// `manifest.tag` value the integration test fixtures emit.
pub const TEST_NODE_TAG: &str = "v1";

/// Builds a node-shaped [`SenderTarget`] with the standard test tag. Panics on
/// invalid names — tests use known-good values only.
pub fn test_node_target(name: &str) -> SenderTarget {
    SenderTarget::node(name, TEST_NODE_TAG).expect("test node target")
}

/// Builds a node-shaped [`SenderTarget`] tagged with [`names::CORE_NODE_TAG`].
/// Use this when the test caller is addressing one of the daemon's own services
/// (clock, info, ping, node_add, …) — the daemon's listeners pin their tag to
/// `CORE_NODE_TAG`, not the `v1` used for ordinary test nodes.
pub fn core_node_target(name: &str) -> SenderTarget {
    SenderTarget::node(name, names::CORE_NODE_TAG).expect("core node target")
}

pub const CALLER_INSTANCE_ID: &str = "caller_instance";
pub const TEST_GIT_HASH: &str = "test-hash";
