#![allow(dead_code)] // Each test binary uses only a subset of these shared helpers.

use super::test_node_target;
use node_stack::NodeStack;
use peppylib::messaging::{MessengerHandle, ServiceTarget};
use peppylib::runtime::TaskHandle;
use std::time::Duration;

/// A wrapper around `TaskHandle` that aborts the task when dropped.
pub struct AbortOnDrop<T>(pub TaskHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Generic polling helper: repeatedly calls `predicate` until it returns
/// `Some(value)`, then returns that value. If `timeout` elapses first, panics
/// with `timeout_message`. Polls every 20 ms. `predicate` is synchronous on
/// purpose: the current callers only touch the filesystem, the node stack, and
/// child processes, none of which await.
pub async fn poll_until<T>(
    timeout: Duration,
    timeout_message: &str,
    mut predicate: impl FnMut() -> Option<T>,
) -> T {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(value) = predicate() {
            return value;
        }
        if std::time::Instant::now() > deadline {
            panic!("{timeout_message}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// True if a process exists and is not a zombie; matches the daemon's own
/// liveness definition (sysinfo, status != Zombie), so tests agree with what
/// `node_stop`/teardown consider "gone". A libc `kill(pid, 0)` check would
/// report a reaped-but-unwaited zombie as still running.
pub fn is_process_running(pid: u32) -> bool {
    let system = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::nothing().with_processes(sysinfo::ProcessRefreshKind::nothing()),
    );
    match system.process(sysinfo::Pid::from_u32(pid)) {
        Some(process) => process.status() != sysinfo::ProcessStatus::Zombie,
        None => false,
    }
}

/// PIDs of live children of `parent_pid`, via sysinfo's parent links.
pub fn children_of(parent_pid: u32) -> Vec<u32> {
    let system = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::nothing().with_processes(sysinfo::ProcessRefreshKind::nothing()),
    );
    let parent = sysinfo::Pid::from_u32(parent_pid);
    system
        .processes()
        .values()
        .filter(|p| p.parent() == Some(parent))
        .map(|p| p.pid().as_u32())
        .collect()
}

/// The state of an instance regardless of lifecycle stage, including terminal
/// (`Finished`/`Failed`) instances that the `Running`-only
/// `NodeStack::find_by_instance_id` no longer returns. Lets a test observe the
/// exit watcher's terminal transition, or confirm an instance is gone from every
/// state (not lingering as terminal after a stop/reset).
pub fn instance_state_in_any_state(
    node_stack: &NodeStack,
    instance_id: &config::runtime::Name,
) -> Option<core_node_api::InstanceState> {
    node_stack.snapshot().into_iter().find_map(|handle| {
        handle
            .read()
            .instances()
            .iter()
            .find(|inst| inst.instance_id() == instance_id)
            .map(|inst| inst.state())
    })
}

/// Polls `ServiceMessenger::is_reachable` until the named service responds or
/// `deadline` expires. Replaces fixed sleeps used as broker-propagation
/// barriers in tests that spawn a `handle_requests` task and then need to
/// be sure callers can route to it.
pub async fn wait_until_service_reachable(
    messenger: &MessengerHandle,
    bound_core_node: &str,
    to_node_name: &str,
    to_service_name: &str,
    target_core_node: &str,
    target_instance_id: &str,
    timeout: Duration,
) {
    use peppylib::messaging::ServiceMessenger;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(true) = ServiceMessenger::is_reachable(
            messenger,
            bound_core_node,
            "ready_probe",
            test_node_target(to_node_name),
            to_service_name,
            ServiceTarget::Producer(&peppylib::messaging::ProducerRef::new(
                target_core_node,
                target_instance_id,
            )),
        )
        .await
        {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "service {to_node_name}/{to_service_name} on \
                 {target_core_node}/{target_instance_id} did not become \
                 reachable within {timeout:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
