//! Signals for the process groups the stack spawns its children into: the
//! host group's SIGTERM and SIGKILL, and for a container on macOS the
//! in-VM group's, together as one [`ProcessGroup`].
//!
//! Every node child is spawned as its own process-group leader (PGID == PID;
//! see `run_steps::spawn_as_process_group_leader`), so one group signal
//! reaches the node and every descendant it forked. `setpgid`/`killpg`
//! semantics are identical on Linux and macOS; off unix the host group
//! signals are no-ops. The test-support items hold a node script that forks
//! a child ignoring SIGTERM, and count a group's members or probe whether it
//! exists.

use std::time::Duration;

#[cfg(unix)]
use tracing::warn;

/// Latency cap on the macOS in-VM guest force-kill phase. The Lima VM
/// liveness check takes up to two 5 s probes and each per-key guest SIGKILL
/// up to 10 s, each under its own `limactl` deadline, so a batch of two keys
/// or more reaches this cap with no call wedged, and the cap also bounds a
/// call that wedges past its deadline. It does not cancel the blocking
/// closure (a `spawn_blocking` task runs to completion regardless); it only
/// stops the caller waiting on it.
pub const GUEST_FORCE_KILL_BUDGET: Duration = Duration::from_secs(30);

/// The process group of an instance's process, as a kill addresses it: the
/// host group its pid leads and, for a container on macOS, the in-VM group
/// keyed by the instance id.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcessGroup {
    pub pid: u32,
    pub is_container: bool,
}

impl ProcessGroup {
    /// SIGKILLs the host group and, for a container, the in-VM group keyed by
    /// `instance_id`.
    pub async fn force_kill(&self, instance_id: &str) {
        kill_process_group(self.pid);
        if self.is_container {
            kill_guest_groups(vec![instance_id.to_owned()]).await;
        }
    }

    /// SIGTERMs the group: the in-VM group keyed by `instance_id` for a
    /// container on macOS, else the host group.
    pub async fn terminate(&self, instance_id: &str) {
        if self.is_container {
            let delivered = terminate_guest_groups(vec![instance_id.to_owned()]).await;
            if delivered {
                return;
            }
        }
        terminate_process_group(self.pid);
    }
}

/// SIGKILLs the in-VM process groups of the container instances `keys` name
/// (macOS), where the host group kill reached only the `limactl` client. The
/// facade owns the platform gate (a no-op on Linux) and never fails: a
/// guest-kill problem must not block a stop, a teardown or an exit. Runs on a
/// blocking thread because it shells out to `limactl`, and waits at most
/// [`GUEST_FORCE_KILL_BUDGET`].
pub async fn kill_guest_groups(keys: Vec<String>) {
    if keys.is_empty() {
        return;
    }
    let _ = tokio::time::timeout(
        GUEST_FORCE_KILL_BUDGET,
        tokio::task::spawn_blocking(move || {
            containers::Apptainer::kill_guest_process_groups_best_effort(&keys);
        }),
    )
    .await;
}

/// SIGTERMs the in-VM process groups of the container instances `keys` name
/// (macOS); `false` when the guest delivered nothing, where the caller
/// SIGTERMs the host groups.
pub async fn terminate_guest_groups(keys: Vec<String>) -> bool {
    tokio::task::spawn_blocking(move || {
        containers::Apptainer::terminate_guest_process_groups_best_effort(&keys)
    })
    .await
    .unwrap_or(false)
}

/// A node's `sh -c` script that forks a child ignoring SIGTERM beside a
/// leader that keeps running, for a test of a group kill.
#[cfg(all(unix, any(test, feature = "test-support")))]
pub const FORKS_A_TERM_IGNORER: &str =
    "sh -c 'trap \"\" TERM; while true; do sleep 0.1; done' & while true; do sleep 0.1; done";

/// How many processes the group `pgid` leads, for a test.
#[cfg(all(unix, any(test, feature = "test-support")))]
pub fn process_group_size(pgid: u32) -> usize {
    let listed = std::process::Command::new("pgrep")
        .args(["-g", &pgid.to_string()])
        .output()
        .expect("pgrep runs");
    String::from_utf8_lossy(&listed.stdout).lines().count()
}

/// Whether any process is left in the group `pgid` leads, for a test.
#[cfg(all(unix, any(test, feature = "test-support")))]
pub fn process_group_exists(pgid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", "--", &format!("-{pgid}")])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// SIGKILLs the entire process group led by `pid`.
#[cfg(unix)]
pub fn kill_process_group(pid: u32) {
    signal_process_group(pid, nix::sys::signal::Signal::SIGKILL);
}

#[cfg(not(unix))]
pub fn kill_process_group(_pid: u32) {}

/// SIGTERMs the entire process group led by `pid`. The daemon uses it as the
/// cooperative fallback for native container runtimes when the Peppy shutdown
/// RPC could not be delivered; its force phase still SIGKILLs any process
/// group that remains.
#[cfg(unix)]
pub fn terminate_process_group(pid: u32) {
    signal_process_group(pid, nix::sys::signal::Signal::SIGTERM);
}

#[cfg(not(unix))]
pub fn terminate_process_group(_pid: u32) {}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: nix::sys::signal::Signal) {
    // A zero pid addresses the caller's own process group, the daemon itself.
    // No instance records one; refusing it here keeps a bug elsewhere from
    // turning into a self-signal.
    let Some(group) = i32::try_from(pid).ok().filter(|pid| *pid > 0) else {
        warn!("Refusing to {signal} process group {pid}: not a spawned child's pid");
        return;
    };
    // `killpg(pgrp, sig)` is POSIX-equivalent to `kill(-pgrp, sig)`: it
    // targets the process group whose PGID == `pid`. nix's safe wrapper keeps
    // the crate free of `unsafe`.
    match nix::sys::signal::killpg(nix::unistd::Pid::from_raw(group), signal) {
        // An already-dead group yields ESRCH; the group is already gone, which
        // is exactly the state we wanted, so treat it as success.
        Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
        // Any other errno (e.g. EPERM) means the signal did not land and the
        // node's process group may still be alive, so surface it.
        Err(err) => warn!("Failed to {signal} node process group (pid {pid}): {err}"),
    }
}
