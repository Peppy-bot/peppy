#![allow(
    dead_code,
    unused_imports,
    reason = "each test binary uses a part of these helpers"
)]

mod actions;
mod daemon;
mod fixtures;
mod lifecycle;
mod poll;
mod services;

// Flat re-exports, so a test calls `common::foo()` whichever group defines
// it; each test binary uses a different subset of the groups.
pub(crate) use actions::*;
pub(crate) use daemon::*;
pub(crate) use fixtures::*;
pub(crate) use lifecycle::*;
pub(crate) use poll::*;
pub(crate) use services::*;

use core_node_api::names;
use peppylib::messaging::SenderTarget;

/// Default tag used by tests when building a [`SenderTarget`]. Matches the
/// `manifest.tag` value the integration test fixtures emit.
pub(crate) const TEST_NODE_TAG: &str = "v1";

/// Serializes real Apptainer tests within the current integration-test binary.
///
/// The tests share one Lima VM whose mount configuration can restart the VM.
/// Each integration-test binary compiles this module separately, so this guard
/// does not coordinate separate test processes.
pub(crate) async fn acquire_container_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    static CONTAINER_TEST_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    CONTAINER_TEST_MUTEX.lock().await
}

/// Builds a node-shaped [`SenderTarget`] with the standard test tag. Panics on
/// invalid names; tests use known-good values only.
pub(crate) fn test_node_target(name: &str) -> SenderTarget {
    SenderTarget::node(name, TEST_NODE_TAG).expect("test node target")
}

/// Builds a node-shaped [`SenderTarget`] tagged with [`names::CORE_NODE_TAG`].
/// Use this when the test caller is addressing one of the daemon's own services
/// (clock, info, ping, `node_add`, …); the daemon's listeners pin their tag to
/// `CORE_NODE_TAG`, not the `v1` used for ordinary test nodes.
pub(crate) fn core_node_target(name: &str) -> SenderTarget {
    SenderTarget::node(name, names::CORE_NODE_TAG).expect("core node target")
}

/// The tests call the daemon as its command line: the harness signs their
/// calls with the key of the command line, which the daemon checks.
pub(crate) const CALLER_INSTANCE_ID: &str = names::COMMAND_LINE_INSTANCE_ID;
pub(crate) const TEST_GIT_HASH: &str = "test-hash";
