//! Test fixtures for setting up `NodeEntity` lifecycle states without
//! invoking the real apptainer/archive build I/O.
//!
//! Production code should always go through `NodeEntity::build` and the
//! `prepare_and_spawn`/`commit_started`/`abort_started` lifecycle. These
//! helpers exist purely so tests can set up `Ready` entities (with optional
//! `Running` instances) without orchestrating the real spawn pipeline.

#![allow(dead_code)] // Some helpers are reserved for future tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use config::node::{Name, NodeConfig};
use node_stack::{InstanceState, NodeStack, NodeStage, TrackedNodeInstance};

/// Process-wide counter so the default fallback instance id is unique
/// across calls (otherwise repeated `push_started`/`start_instance_in_stack`
/// invocations would collide on `Name::new("test-instance")` and trip the
/// `DuplicateInstanceId` check exposed by the lifecycle path).
static FALLBACK_INSTANCE_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn fallback_instance_name() -> Name {
    let n = FALLBACK_INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    Name::new(format!("test-instance-{}", n)).expect("test fixture: name")
}

/// Pushes a config into the stack and immediately marks the resulting entity
/// as `Ready` with an empty instances list (the equivalent of the old
/// `Built` stage). The artifact path is set to the same value as
/// `config_path` for convenience — tests don't actually read it.
pub fn push_built(stack: &NodeStack, config: NodeConfig, path: impl Into<PathBuf>) {
    let path = path.into();
    let name = config.manifest.name.as_str().to_owned();
    let tag = config.manifest.tag.clone();

    stack
        .push_config(config, false, &path)
        .expect("test fixture: push_config should succeed");

    let handle = stack
        .find(&name, &tag)
        .expect("test fixture: just-pushed entity should exist");
    handle
        .write()
        .expect("entity poisoned")
        .__test_set_stage(NodeStage::Ready {
            config_path: path.clone(),
            artifact_path: path,
            instances: Vec::new(),
        });
}

/// Pushes a config and marks it `Ready` with a single `Running` instance —
/// the equivalent of the old `Started` stage with one instance.
pub fn push_started(
    stack: &NodeStack,
    config: NodeConfig,
    path: impl Into<PathBuf>,
    instance_id: Option<&Name>,
    pid: Option<u32>,
) -> Name {
    let path = path.into();
    let name = config.manifest.name.as_str().to_owned();
    let tag = config.manifest.tag.clone();

    stack
        .push_config(config, false, &path)
        .expect("test fixture: push_config should succeed");

    let handle = stack
        .find(&name, &tag)
        .expect("test fixture: just-pushed entity should exist");

    let instance_id = match instance_id {
        Some(id) => id.clone(),
        None => fallback_instance_name(),
    };
    let instance = TrackedNodeInstance::new(instance_id.clone(), pid, InstanceState::Running);
    handle
        .write()
        .expect("entity poisoned")
        .__test_set_stage(NodeStage::Ready {
            config_path: path.clone(),
            artifact_path: path,
            instances: vec![instance],
        });
    instance_id
}

/// Appends a `Running` instance to a `Ready` entity at `(name, tag)`.
/// Equivalent to the old `start_instance` test pattern, but writes directly
/// into the instances list via the `__test_stage_mut` backdoor instead of
/// going through a production lifecycle method.
pub fn start_instance_in_stack(
    stack: &NodeStack,
    name: &str,
    tag: &str,
    instance_id: Option<&Name>,
    pid: Option<u32>,
) -> Name {
    let handle = stack
        .find(name, tag)
        .expect("test fixture: entity should exist for start_instance_in_stack");
    let instance_id = match instance_id {
        Some(id) => id.clone(),
        None => fallback_instance_name(),
    };
    let instance = TrackedNodeInstance::new(instance_id.clone(), pid, InstanceState::Running);
    let mut guard = handle.write().expect("entity poisoned");
    let NodeStage::Ready { instances, .. } = guard.__test_stage_mut() else {
        panic!("test fixture: start_instance_in_stack requires Ready stage");
    };
    instances.push(instance);
    instance_id
}

/// Calls `stop_instance` on the entity at `(name, tag)`. Returns whether an
/// instance was actually removed.
pub fn stop_instance_in_stack(
    stack: &NodeStack,
    name: &str,
    tag: &str,
    instance_id: &Name,
) -> bool {
    let Some(handle) = stack.find(name, tag) else {
        return false;
    };
    handle
        .write()
        .expect("entity poisoned")
        .stop_instance(instance_id)
}

/// Returns the path to a (deterministic) fake `.sif` for tests. The path is
/// not created on disk — it's just a placeholder used by `push_built` /
/// `__test_set_stage` to fake the artifact location.
pub fn fake_sif_path(name: &str, tag: &str) -> PathBuf {
    Path::new("/tmp")
        .join("peppy-test")
        .join(format!("{}_{}.sif", name, tag))
}
