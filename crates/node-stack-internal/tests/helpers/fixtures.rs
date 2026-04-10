//! Thin async wrappers around [`real_lifecycle`] helpers, preserved so test
//! modules can migrate incrementally.
//!
//! Every call that formerly produced a `Ready` entity now spawns a real
//! subprocess (a portable `sh` loop), so callers must hold onto the returned
//! [`RunningInstanceGuard`] for the duration of the test — dropping it calls
//! `stop_instance` and SIGTERMs the child.

#![allow(dead_code)] // Some helpers are reserved for future tests.

use config::node::{Name, NodeConfig};
use node_stack::{EntityHandle, NodeStack};

use crate::helpers::real_lifecycle::{
    self, LifecycleHarness, RunningInstanceGuard, fixture_instance_name,
};

/// Pushes a config and drives the real `build` lifecycle so the resulting
/// entity lands in `Ready { instances: [] }`. The `execution` fields of the
/// config are overridden by the fixture (see
/// [`real_lifecycle::override_execution_for_fixture`]) so tests must not
/// assert on `run_cmd`/`container`/`build_cmd` after this call.
pub async fn push_built(
    stack: &NodeStack,
    harness: &LifecycleHarness,
    config: NodeConfig,
) -> EntityHandle {
    let config_path = harness.peppy_root.path().join("fixture_config.json5");
    real_lifecycle::build_ready(stack, harness, config, config_path).await
}

/// Pushes a config, builds, and spawns one `Running` instance — the
/// equivalent of the old `Started` stage with one instance. Returns the
/// guard; drop it to clean up the child process.
pub async fn push_started(
    stack: &NodeStack,
    harness: &LifecycleHarness,
    config: NodeConfig,
    instance_id: Option<&Name>,
) -> RunningInstanceGuard {
    let handle = push_built(stack, harness, config).await;
    let instance_id = instance_id.cloned().unwrap_or_else(fixture_instance_name);
    real_lifecycle::spawn_running_instance(handle, harness, instance_id).await
}

/// Spawns a real `Running` instance on the existing entity at `(name, tag)`.
/// Caller must hold the returned guard for the test lifetime.
pub async fn start_instance_in_stack(
    stack: &NodeStack,
    harness: &LifecycleHarness,
    name: &str,
    tag: &str,
    instance_id: Option<&Name>,
) -> RunningInstanceGuard {
    let handle = stack
        .find(name, tag)
        .expect("test fixture: entity should exist for start_instance_in_stack");
    let instance_id = instance_id.cloned().unwrap_or_else(fixture_instance_name);
    real_lifecycle::spawn_running_instance(handle, harness, instance_id).await
}

/// Calls `stop_instance` on the entity at `(name, tag)`. Returns whether an
/// instance was actually removed. Idempotent with the guard's Drop cleanup:
/// a subsequent drop will just re-call `stop_instance` (returning `false`)
/// and send SIGTERM to the already-stopped child.
pub fn stop_instance_in_stack(
    stack: &NodeStack,
    name: &str,
    tag: &str,
    instance_id: &Name,
) -> bool {
    let Some(handle) = stack.find(name, tag) else {
        return false;
    };
    handle.write().stop_instance(instance_id)
}
