#![allow(dead_code)] // Each test binary uses only a subset of these shared helpers.

use super::test_node_target;
use peppylib::messaging::{MessengerHandle, ServiceTarget};
use peppylib::runtime::TaskHandle;
use std::time::Duration;

/// How long a test waits for one event: a state, a stack log line, the exit
/// of a process, the answer to a request that the daemon answers without
/// long work. A test that waits for a request with a bound of the daemon
/// waits that bound, then this. No event of these tests comes near it on a
/// host of any speed: a test runs into it only when the daemon does not act.
pub const WAIT: Duration = Duration::from_secs(15);

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
    predicate: impl FnMut() -> Option<T>,
) -> T {
    poll_until_explained(timeout, || timeout_message.to_owned(), predicate).await
}

/// [`poll_until`] whose timeout message is built when `timeout` elapses, so
/// it reports the state at that moment.
pub async fn poll_until_explained<T>(
    timeout: Duration,
    timeout_message: impl FnOnce() -> String,
    mut predicate: impl FnMut() -> Option<T>,
) -> T {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(value) = predicate() {
            return value;
        }
        if std::time::Instant::now() > deadline {
            panic!("{}", timeout_message());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Every process of the machine, with its status and parent.
fn process_table() -> sysinfo::System {
    sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::nothing().with_processes(sysinfo::ProcessRefreshKind::nothing()),
    )
    .expect("sysinfo reads the process table of Linux and macOS")
}

/// True if a process exists and is not a zombie; matches the daemon's own
/// liveness definition (sysinfo, status != Zombie), so tests agree with what
/// `node_stop`/teardown consider "gone". A libc `kill(pid, 0)` check would
/// report a reaped-but-unwaited zombie as still running.
pub fn is_process_running(pid: u32) -> bool {
    let system = process_table();
    match system.process(sysinfo::Pid::from_u32(pid)) {
        Some(process) => process.status() != sysinfo::ProcessStatus::Zombie,
        None => false,
    }
}

/// Polls until the process `pid` has exited (absent or a zombie, as
/// [`is_process_running`] defines it). If `timeout` elapses first, panics with
/// `timeout_message`.
pub async fn wait_until_process_exits(pid: u32, timeout: Duration, timeout_message: &str) {
    poll_until(timeout, timeout_message, || {
        (!is_process_running(pid)).then_some(())
    })
    .await
}

/// PIDs of live children of `parent_pid`, via sysinfo's parent links.
pub fn children_of(parent_pid: u32) -> Vec<u32> {
    let system = process_table();
    let parent = sysinfo::Pid::from_u32(parent_pid);
    system
        .processes()
        .values()
        .filter(|p| p.parent() == Some(parent))
        .map(|p| p.pid().as_u32())
        .collect()
}

/// Polls until the node process `node_pid` has the two children that a node
/// from [`add_and_build_forking_node`](super::add_and_build_forking_node)
/// forks, and returns their pids.
pub async fn wait_for_forked_grandchildren(node_pid: u32) -> Vec<u32> {
    poll_until(WAIT, "expected the node to fork two grandchildren", || {
        let children = children_of(node_pid);
        (children.len() >= 2).then_some(children)
    })
    .await
}

/// Polls until every process in `grandchildren` has exited, each within
/// [`WAIT`]. `killed_by` names the action whose group kill must have reaped
/// them, for the timeout message.
pub async fn wait_until_grandchildren_exit(grandchildren: &[u32], killed_by: &str) {
    for &grandchild in grandchildren {
        wait_until_process_exits(
            grandchild,
            WAIT,
            &format!("grandchild {grandchild} should be gone after {killed_by} (group kill)"),
        )
        .await;
    }
}

/// Polls `ServiceMessenger::is_reachable` until the named service responds or
/// [`WAIT`] expires. It is the broker-propagation barrier for tests that
/// spawn a `handle_requests` task and then need to be sure callers can route
/// to it.
pub async fn wait_until_service_reachable(
    messenger: &MessengerHandle,
    bound_core_node: &str,
    to_node_name: &str,
    to_service_name: &str,
    target_core_node: &str,
    target_instance_id: &str,
) {
    use peppylib::messaging::ServiceMessenger;
    let deadline = std::time::Instant::now() + WAIT;
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
                 reachable within {WAIT:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
