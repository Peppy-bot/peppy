//! Dropping the last handle to a `NodeStack` kills the children it tracks.
//!
//! The stack is the daemon's only record of the processes it spawned, so when
//! its last clone goes away without a cooperative teardown (a serve task
//! aborted, a test runtime shutting down) the children must go with it rather
//! than outlive the daemon as orphans. Each test spawns a real child through
//! the real lifecycle and keeps the `Child` handle, so the exit status it
//! reaps is the proof: `SIGKILL` from the stack, or a clean exit of its own.

use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::time::Duration;

use config::runtime::Name;
use nix::sys::signal::Signal;
use node_stack::{EntityHandle, NodeEntity, NodeStack, StartContext, StartedInstanceCtx};
use tokio::process::Child;

use crate::helpers::config_common::core_node_config;
use crate::helpers::real_lifecycle::{self, LifecycleHarness};

/// Hang guard only. The drop sends its signal synchronously before the wait
/// starts, so a passing run never depends on this bound; a broken drop turns
/// into a failed assertion instead of a hung test.
const REAP_BOUND: Duration = Duration::from_secs(30);

fn sensor_config() -> config::node::NodeConfig {
    serde_json5::from_str(
        r#"{
            peppy_schema: "node/v1",
            manifest: { name: "sensor", tag: "v1" },
            execution: { language: "rust", run_cmd: ["sensor"] }
        }"#,
    )
    .expect("valid sensor config")
}

/// A `Ready` sensor entity in `stack` with one child spawned for
/// `instance_id`, still `Starting`: `prepare_and_spawn` ran, `commit_started`
/// did not.
async fn spawn_starting(
    stack: &NodeStack,
    harness: &LifecycleHarness,
    instance_id: &Name,
) -> (EntityHandle, Child, StartedInstanceCtx) {
    let config_path = harness.peppy_root.path().join("sensor.json5");
    let handle = real_lifecycle::build_ready(stack, harness, sensor_config(), config_path).await;
    let (child, started_ctx) = NodeEntity::prepare_and_spawn(
        &handle,
        StartContext {
            instance_id,
            runtime_config_json5: "{}",
            slot_bindings: std::collections::BTreeMap::new(),
            env_vars: &[],
            mount_paths_resolved: &[],
            peppy_dirs: &harness.peppy_dirs,
            output_sinks: harness.output_sinks(),
        },
    )
    .await
    .expect("prepare_and_spawn should succeed on a Ready entity");
    (handle, child, started_ctx)
}

/// Reaps `child` and returns its exit status, failing rather than hanging if
/// it never exits.
async fn reap(child: &mut Child) -> std::process::ExitStatus {
    tokio::time::timeout(REAP_BOUND, child.wait())
        .await
        .expect("the child should exit once the stack dropped it")
        .expect("wait on the child")
}

#[tokio::test]
async fn dropping_the_last_stack_handle_kills_a_running_instance() {
    let harness = real_lifecycle::lifecycle_harness();
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let instance_id = Name::new("dropped-running").unwrap();
    let (handle, child, started_ctx) = spawn_starting(&stack, &harness, &instance_id).await;
    let mut child = NodeEntity::commit_started(&handle, child, started_ctx, instance_id)
        .await
        .expect("commit_started should succeed");

    drop(stack);

    let status = reap(&mut child).await;
    assert_eq!(
        status.signal(),
        Some(Signal::SIGKILL as i32),
        "the stack must SIGKILL its running instance when its last handle drops; got {status}"
    );
}

#[tokio::test]
async fn dropping_the_last_stack_handle_kills_an_instance_still_starting() {
    let harness = real_lifecycle::lifecycle_harness();
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let instance_id = Name::new("dropped-starting").unwrap();
    let (_handle, mut child, _started_ctx) = spawn_starting(&stack, &harness, &instance_id).await;

    drop(stack);

    let status = reap(&mut child).await;
    assert_eq!(
        status.signal(),
        Some(Signal::SIGKILL as i32),
        "a child spawned but not yet committed is tracked too and must die with the stack; \
         got {status}"
    );
}

#[tokio::test]
async fn a_surviving_stack_handle_keeps_its_instances_alive() {
    let harness = real_lifecycle::lifecycle_harness();
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let instance_id = Name::new("kept-by-clone").unwrap();
    let (handle, child, started_ctx) = spawn_starting(&stack, &harness, &instance_id).await;
    let mut child = NodeEntity::commit_started(&handle, child, started_ctx, instance_id.clone())
        .await
        .expect("commit_started should succeed");
    let pid = child.id().expect("the child is still running");

    let survivor = stack.clone();
    drop(stack);

    // End the instance with a SIGTERM of our own. A process the first drop had
    // SIGKILLed can only ever report SIGKILL: the kill lands before this signal
    // is sent, and a fatally signaled process runs nothing further. So either
    // outcome of our SIGTERM proves the instance was untouched: the fixture's
    // `trap 'exit 0' TERM` when the shell had installed it, the raw signal when
    // it had not yet.
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), Signal::SIGTERM)
        .expect("the instance should still be alive while a stack handle survives");
    let status = reap(&mut child).await;
    assert!(
        status.code() == Some(0) || status.signal() == Some(Signal::SIGTERM as i32),
        "the instance should have ended on our SIGTERM, not on a SIGKILL from the drop; \
         got {status}"
    );

    // The process is gone, so take it out of the stack before the last
    // handle drops: the drop signals every pid it still tracks.
    assert!(handle.write().stop_instance(&instance_id));
    drop(survivor);
}

#[test]
fn the_root_instance_records_no_pid() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let root = stack.root();
    let guard = root.read();
    let [root_instance] = guard.instances() else {
        panic!("the root entity tracks exactly its own instance");
    };
    assert_eq!(
        root_instance.pid(),
        None,
        "the daemon is its own process and the stack must never signal it"
    );
}
