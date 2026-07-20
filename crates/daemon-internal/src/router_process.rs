//! Durable process fence for interrupted managed-router logout.
//!
//! A daemon can be killed after remote revocation but before it has made its
//! child router standalone. Before the logout intent is written, we capture
//! the exact child PID, process birth time, executable, effective UID, and
//! `-c` argument in [`DaemonState`]. A replacement daemon revalidates all of
//! those fields immediately before signaling the orphan. Ambiguous state is a
//! startup error: identity material is retained rather than deleted while an
//! unproven process may still hold it in memory.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::time::{Duration, Instant};

#[cfg(not(target_os = "linux"))]
use sysinfo::Signal;
use sysinfo::{Pid, Process, ProcessStatus, ProcessesToUpdate, System};
use tracing::{info, warn};

use crate::error::{Error, Result};
use crate::state::{DaemonState, ManagedRouterLaunch, ManagedRouterProcess, RouterOwnership};

const ROUTER_EXIT_WAIT: Duration = Duration::from_secs(2);
const ROUTER_EXIT_POLL: Duration = Duration::from_millis(25);

/// Reusable description of the daemon state/config pair whose current direct
/// child must be fenced. Cloned into every code path that can spawn or restart
/// the managed router.
#[derive(Clone)]
pub(crate) struct RouterProcessRecorder {
    state_path: std::path::PathBuf,
    launch: ManagedRouterLaunch,
}

impl RouterProcessRecorder {
    pub(crate) fn new(
        state_path: std::path::PathBuf,
        config_path: std::path::PathBuf,
    ) -> Result<Self> {
        let daemon_pid = std::process::id();
        let system = System::new_all();
        let process = system.process(Pid::from_u32(daemon_pid)).ok_or_else(|| {
            Error::ExecutionFailed(
                "cannot inspect the daemon process before managed-router startup".into(),
            )
        })?;
        let session_id = process.session_id().ok_or_else(|| {
            Error::ExecutionFailed(
                "cannot identify the daemon session before managed-router startup".into(),
            )
        })?;
        let process_group_id = rustix::process::getpgrp();
        Ok(Self {
            state_path,
            launch: ManagedRouterLaunch {
                daemon_pid,
                daemon_start_time_unix: process.start_time(),
                effective_uid: current_effective_uid(),
                process_group_id: process_group_id.as_raw_pid() as u32,
                session_id: session_id.as_u32(),
                config_path,
            },
        })
    }

    pub(crate) fn capture_current(&self) -> Result<()> {
        capture_current(&self.state_path, &self.launch)
    }

    pub(crate) fn launch_descriptor(&self) -> ManagedRouterLaunch {
        self.launch.clone()
    }
}

fn execution_uid(process: &Process) -> Option<u32> {
    process.effective_user_id().map(|uid| **uid)
}

fn is_zenohd_executable(process: &Process) -> bool {
    process
        .exe()
        .and_then(Path::file_name)
        .is_some_and(|name| name == OsStr::new("zenohd"))
}

fn command_uses_exact_config(command: &[OsString], config_path: &Path) -> bool {
    let mut config_flags = command
        .iter()
        .enumerate()
        .filter(|(_, argument)| argument.as_os_str() == OsStr::new("-c"));
    let Some((index, _)) = config_flags.next() else {
        return false;
    };
    config_flags.next().is_none()
        && command
            .get(index + 1)
            .is_some_and(|value| value.as_os_str() == config_path.as_os_str())
}

fn matches_config_and_owner(process: &Process, config_path: &Path, effective_uid: u32) -> bool {
    is_zenohd_executable(process)
        && execution_uid(process) == Some(effective_uid)
        && command_uses_exact_config(process.cmd(), config_path)
}

fn matches_fence(process: &Process, fence: &ManagedRouterProcess) -> bool {
    process.pid().as_u32() == fence.pid
        && process.start_time() == fence.start_time_unix
        && execution_uid(process) == Some(fence.effective_uid)
        && process.exe() == Some(fence.executable.as_path())
        && is_zenohd_executable(process)
        && command_uses_exact_config(process.cmd(), &fence.config_path)
}

fn process_group_id(process: &Process) -> Option<u32> {
    let raw = i32::try_from(process.pid().as_u32()).ok()?;
    let pid = rustix::process::Pid::from_raw(raw)?;
    rustix::process::getpgid(Some(pid))
        .ok()
        .map(|group| group.as_raw_pid() as u32)
}

fn matches_launch(process: &Process, launch: &ManagedRouterLaunch) -> bool {
    matches_config_and_owner(process, &launch.config_path, launch.effective_uid)
        && process.start_time() >= launch.daemon_start_time_unix
        && process_group_id(process) == Some(launch.process_group_id)
        && process.session_id().map(Pid::as_u32) == Some(launch.session_id)
}

fn current_effective_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// Captures and durably publishes the exact direct child for this launch. It is
/// called after every successful start/restart and again immediately before
/// auth writes a logout intent.
pub(crate) fn capture_current(state_path: &Path, launch: &ManagedRouterLaunch) -> Result<()> {
    let mut state = DaemonState::read_from(state_path).map_err(|error| {
        Error::ExecutionFailed(format!(
            "cannot read daemon state while recording the managed router: {error}"
        ))
    })?;
    let daemon_pid = std::process::id();
    if state.daemon_pid != Some(daemon_pid)
        || state.router_external
        || !matches!(
            state.router_ownership,
            RouterOwnership::PeppyManaged | RouterOwnership::OperatorManaged
        )
    {
        return Err(Error::ExecutionFailed(
            "daemon state does not identify this generation as the managed-router owner".into(),
        ));
    }
    if state.managed_router_launch.as_ref() != Some(launch) {
        return Err(Error::ExecutionFailed(
            "daemon state does not contain this generation's managed-router launch fence".into(),
        ));
    }

    let system = System::new_all();
    let parent = Pid::from_u32(daemon_pid);
    let candidates: Vec<&Process> = system
        .processes()
        .values()
        .filter(|process| process.parent() == Some(parent) && matches_launch(process, launch))
        .collect();
    let [candidate] = candidates.as_slice() else {
        return Err(Error::ExecutionFailed(format!(
            "expected exactly one owned zenohd child, found {}",
            candidates.len()
        )));
    };
    let executable = candidate.exe().expect("validated executable").to_path_buf();
    let fence = ManagedRouterProcess {
        pid: candidate.pid().as_u32(),
        start_time_unix: candidate.start_time(),
        effective_uid: launch.effective_uid,
        executable,
        config_path: launch.config_path.clone(),
    };

    // Refresh from scratch before publishing so a child that exited or was
    // replaced during the scan cannot leave a stale fence at the commit point.
    let refreshed = System::new_all();
    if !refreshed
        .process(Pid::from_u32(fence.pid))
        .is_some_and(|process| {
            process.parent() == Some(parent)
                && matches_launch(process, launch)
                && matches_fence(process, &fence)
        })
    {
        return Err(Error::ExecutionFailed(
            "managed zenohd changed while its logout process fence was being captured".into(),
        ));
    }

    state = state.with_managed_router_process(fence);
    DaemonState::write_to(state_path, &state).map_err(|error| {
        Error::ExecutionFailed(format!(
            "cannot persist the managed-router process fence: {error}"
        ))
    })
}

fn launch_candidates<'a>(system: &'a System, launch: &ManagedRouterLaunch) -> Vec<&'a Process> {
    system
        .processes()
        .values()
        .filter(|process| matches_launch(process, launch))
        .collect()
}

fn fence_from_process(process: &Process, launch: &ManagedRouterLaunch) -> ManagedRouterProcess {
    ManagedRouterProcess {
        pid: process.pid().as_u32(),
        start_time_unix: process.start_time(),
        effective_uid: launch.effective_uid,
        executable: process
            .exe()
            .expect("validated zenohd executable")
            .to_path_buf(),
        config_path: launch.config_path.clone(),
    }
}

/// Stops the exact router orphan captured for an interrupted logout. External
/// and unmanaged routers are outside Peppy's lifecycle. Any mismatch aborts
/// recovery without deleting credentials or key material.
pub(crate) fn stop_before_startup(state_path: &Path, process_fence_required: bool) -> Result<()> {
    let state = match DaemonState::read_from(state_path) {
        Ok(state) => state,
        Err(error) if !process_fence_required && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(error) => {
            return Err(Error::ExecutionFailed(format!(
                "cannot read prior daemon state for managed-router recovery: {error}"
            )));
        }
    };
    if state.router_external || state.router_ownership == RouterOwnership::Unmanaged {
        return Ok(());
    }
    let launch = state.managed_router_launch;
    if launch
        .as_ref()
        .is_some_and(|launch| launch.effective_uid != current_effective_uid())
        || state
            .managed_router_process
            .as_ref()
            .is_some_and(|fence| fence.effective_uid != current_effective_uid())
    {
        return Err(Error::ExecutionFailed(
            "interrupted logout router fence belongs to a different user".into(),
        ));
    }

    let system = System::new_all();
    let exact = state.managed_router_process.and_then(|fence| {
        system
            .process(Pid::from_u32(fence.pid))
            .filter(|process| {
                matches_fence(process, &fence)
                    && launch
                        .as_ref()
                        .is_none_or(|launch| matches_launch(process, launch))
            })
            .map(|_| fence)
    });
    let fence = if let Some(fence) = exact {
        fence
    } else if let Some(launch) = launch.as_ref() {
        let candidates = launch_candidates(&system, launch);
        match candidates.as_slice() {
            [candidate] => fence_from_process(candidate, launch),
            [] => {
                let ambiguous = system.processes().values().any(|process| {
                    matches_config_and_owner(process, &launch.config_path, launch.effective_uid)
                });
                if ambiguous {
                    return Err(Error::ExecutionFailed(
                        "a zenohd uses the prior managed config but does not match its launch generation; refusing to signal it"
                            .into(),
                    ));
                }
                info!(
                    event = "identity_logout_orphan_fence",
                    outcome = "already_stopped",
                    "the prior generation's managed router is no longer running"
                );
                return Ok(());
            }
            _ => {
                return Err(Error::ExecutionFailed(
                    "multiple zenohd processes match the prior managed-router launch fence".into(),
                ));
            }
        }
    } else {
        if process_fence_required {
            return Err(Error::ExecutionFailed(
                "interrupted logout has no managed-router launch fence; refusing local identity deletion"
                    .into(),
            ));
        }
        return Ok(());
    };
    let pid = Pid::from_u32(fence.pid);

    // Linux pidfds bind the eventual signal to this exact process instance,
    // closing the revalidate/kill PID-reuse window. Platforms without a stable
    // process handle still use the strict birth/executable/argv revalidation
    // immediately before their native signal below.
    #[cfg(target_os = "linux")]
    let process_handle = {
        let raw_pid = i32::try_from(fence.pid).map_err(|_| {
            Error::ExecutionFailed("managed-router PID does not fit the platform PID type".into())
        })?;
        let process_pid = rustix::process::Pid::from_raw(raw_pid).ok_or_else(|| {
            Error::ExecutionFailed("managed-router PID is not a valid process identifier".into())
        })?;
        rustix::process::pidfd_open(process_pid, rustix::process::PidfdFlags::empty()).map_err(
            |error| {
                Error::ExecutionFailed(format!(
                    "cannot acquire a stable handle for the fenced managed router: {error}"
                ))
            },
        )?
    };

    // Re-read immediately before signaling. On Linux this validates the process
    // after its stable pidfd has been acquired; elsewhere it narrows the native
    // PID signaling window as far as the platform API permits.
    let mut refreshed = System::new_all();
    if !refreshed.process(pid).is_some_and(|process| {
        matches_fence(process, &fence)
            && launch
                .as_ref()
                .is_none_or(|launch| matches_launch(process, launch))
    }) {
        return Err(Error::ExecutionFailed(
            "the interrupted logout router changed before it could be stopped".into(),
        ));
    }
    #[cfg(target_os = "linux")]
    rustix::process::pidfd_send_signal(&process_handle, rustix::process::Signal::KILL).map_err(
        |error| {
            Error::ExecutionFailed(format!(
                "the operating system refused to stop the fenced managed router: {error}"
            ))
        },
    )?;
    #[cfg(not(target_os = "linux"))]
    match refreshed
        .process(pid)
        .and_then(|process| process.kill_with(Signal::Kill))
    {
        Some(true) => {}
        Some(false) => {
            return Err(Error::ExecutionFailed(
                "the operating system refused to stop the fenced managed router".into(),
            ));
        }
        None => {
            return Err(Error::ExecutionFailed(
                "the operating system does not support the required managed-router kill signal"
                    .into(),
            ));
        }
    }

    let started = Instant::now();
    loop {
        refreshed.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        match refreshed.process(pid) {
            None => break,
            Some(process)
                if process.start_time() != fence.start_time_unix
                    || process.status() == ProcessStatus::Zombie =>
            {
                break;
            }
            Some(_) if started.elapsed() < ROUTER_EXIT_WAIT => {
                std::thread::sleep(ROUTER_EXIT_POLL);
            }
            Some(_) => {
                warn!(
                    event = "identity_logout_orphan_fence",
                    outcome = "still_running",
                    "fenced managed router did not exit within its bounded recovery wait"
                );
                return Err(Error::ExecutionFailed(
                    "the fenced managed router did not stop; refusing local identity deletion"
                        .into(),
                ));
            }
        }
    }
    info!(
        event = "identity_logout_orphan_fence",
        outcome = "stopped",
        "stopped the exact managed router left by an interrupted logout"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn config_argument_match_is_exact_and_unambiguous() {
        let path = Path::new("/tmp/peppy-router.json5");
        assert!(command_uses_exact_config(
            &args(&["/bin/zenohd", "-c", "/tmp/peppy-router.json5"]),
            path
        ));
        assert!(!command_uses_exact_config(
            &args(&["/bin/zenohd", "--config", "/tmp/peppy-router.json5"]),
            path
        ));
        assert!(!command_uses_exact_config(
            &args(&["/bin/zenohd", "-c", "/tmp/peppy-router.json5.backup"]),
            path
        ));
        assert!(!command_uses_exact_config(
            &args(&[
                "/bin/zenohd",
                "-c",
                "/tmp/peppy-router.json5",
                "-c",
                "/tmp/peppy-router.json5",
            ]),
            path
        ));
        assert!(!command_uses_exact_config(
            &args(&[
                "/bin/zenohd",
                "-c",
                "/tmp/peppy-router.json5",
                "-c",
                "/tmp/another-router.json5",
            ]),
            path
        ));
        assert!(!command_uses_exact_config(
            &args(&["/bin/zenohd", "-c"]),
            path
        ));
    }

    #[test]
    fn external_router_recovery_never_requires_or_signals_a_child() {
        let dir = tempfile::tempdir().unwrap();
        let path = DaemonState::state_file_in(dir.path());
        let state = DaemonState::new(
            "core",
            "router.example",
            7447,
            "test",
            5,
            config::namespace::Namespace::local(),
            RouterOwnership::OperatorManaged,
            Some(30),
        )
        .with_router_external(true);
        DaemonState::write_to(&path, &state).unwrap();
        stop_before_startup(&path, true).unwrap();
    }

    #[test]
    fn managed_router_recovery_refuses_missing_process_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = DaemonState::state_file_in(dir.path());
        let state = DaemonState::new(
            "core",
            "127.0.0.1",
            7447,
            "test",
            5,
            config::namespace::Namespace::local(),
            RouterOwnership::PeppyManaged,
            Some(30),
        );
        DaemonState::write_to(&path, &state).unwrap();
        assert!(stop_before_startup(&path, true).is_err());
        stop_before_startup(&path, false).unwrap();
    }
}
