//! Test fixtures for setting up `NodeEntity` lifecycle states without
//! invoking the real apptainer/archive build I/O.
//!
//! Production code should always go through `NodeEntity::build`. These
//! helpers are for tests only.

#![allow(dead_code)] // Some helpers are reserved for future tests.

use std::path::{Path, PathBuf};

use config::node::{Name, NodeConfig};
use node_stack::{NodeStack, TrackedNodeInstance};

/// Pushes a config into the stack and immediately marks the resulting entity
/// as `Built` with the same `path` (no I/O). Returns nothing — call
/// [`NodeStack::find`] to retrieve the handle.
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
        .restore_built(path)
        .expect("test fixture: restore_built should succeed on a fresh Added entity");
}

/// Pushes a config, marks it Built, and registers a single instance — the
/// equivalent of the old `push_config + add_instance` pattern that pre-dated
/// the lifecycle refactor.
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

    push_built(stack, config, &path);
    let handle = stack
        .find(&name, &tag)
        .expect("test fixture: just-pushed entity should exist");

    let instance_id = match instance_id {
        Some(id) => id.clone(),
        None => Name::new("test-instance").expect("test fixture: name"),
    };
    let instance = TrackedNodeInstance::new(instance_id.clone(), pid);
    handle
        .write()
        .expect("entity poisoned")
        .start_instance(instance)
        .expect("test fixture: start_instance should succeed on a Built entity");
    instance_id
}

/// Convenience for tests that just want to call `start_instance` on an
/// already-built entity. Looks up the entity in `stack` by `(name, tag)` and
/// applies the transition. Returns the instance id (generating "test-instance"
/// if `instance_id` is None).
pub fn start_instance_in_stack(
    stack: &NodeStack,
    name: &str,
    tag: &str,
    instance_id: Option<&Name>,
    pid: Option<u32>,
) -> Name {
    let handle = stack
        .find(name, tag)
        .expect("test fixture: entity should exist for start_instance");
    let instance_id = match instance_id {
        Some(id) => id.clone(),
        None => Name::new("test-instance").expect("test fixture: name"),
    };
    let instance = TrackedNodeInstance::new(instance_id.clone(), pid);
    handle
        .write()
        .expect("entity poisoned")
        .start_instance(instance)
        .expect("test fixture: start_instance should succeed");
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
/// not created on disk — it's just a placeholder for `restore_built`.
pub fn fake_sif_path(name: &str, tag: &str) -> PathBuf {
    Path::new("/tmp")
        .join("peppy-test")
        .join(format!("{}_{}.sif", name, tag))
}
