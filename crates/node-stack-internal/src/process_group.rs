//! Signals for the process groups the stack spawns its children into.
//!
//! Every node child is spawned as its own process-group leader (PGID == PID;
//! see `run_steps::spawn_as_process_group_leader`), so one group signal
//! reaches the node and every descendant it forked. `setpgid`/`killpg`
//! semantics are identical on Linux and macOS; off unix both entry points are
//! no-ops.

#[cfg(unix)]
use tracing::warn;

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
