use std::path::{Path, PathBuf};

use daemon_config::consts::PeppyDirs;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};

const DAEMON_STATE_FILENAME: &str = "daemon_state.json5";

/// Ownership of the router process/config used by this daemon generation.
/// This is explicit because identity-control availability no longer implies
/// Peppy can rewrite the router: external routers run the same controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouterOwnership {
    PeppyManaged,
    OperatorManaged,
    Unmanaged,
}

/// Exact identity of a Peppy-spawned router process captured immediately
/// before the durable logout commit point. Startup recovery uses every field
/// to distinguish the orphan from PID reuse or an unrelated same-user
/// `zenohd`; the config argument alone is never sufficient authority to kill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedRouterProcess {
    pub pid: u32,
    /// Process birth time in Unix seconds as reported by the operating system.
    pub start_time_unix: u64,
    pub effective_uid: u32,
    pub executable: PathBuf,
    pub config_path: PathBuf,
}

/// Identity of the daemon generation that is about to spawn a managed router.
/// Written before the child can exist, so even a kill during PMI's readiness
/// wait can be recovered without trusting a reusable config path alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedRouterLaunch {
    pub daemon_pid: u32,
    pub daemon_start_time_unix: u64,
    pub effective_uid: u32,
    pub process_group_id: u32,
    pub session_id: u32,
    pub config_path: PathBuf,
}

/// Persistent state for the peppy daemon.
///
/// This struct is serialized to JSON5 and stored on disk to track daemon state
/// across restarts. The state file lives at the peppy data root
/// (`~/.peppy/daemon_state.json5` in production, a temp-dir root in
/// development); `PEPPY_HOME` relocates the whole root and the state file
/// with it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonState {
    /// The name of the node currently acting as the core node.
    pub core_node_name: String,
    pub daemon_pid: Option<u32>,
    /// The dial host the daemon, CLI, and spawned nodes use for the messaging
    /// router.
    pub messaging_host: String,
    /// The dial port the messaging router is serving.
    pub messaging_port: u16,
    /// The git hash of the peppy binary at compile time.
    pub git_hash: String,
    /// Cooperative-shutdown grace period the daemon resolved from
    /// `peppy_config.lifecycle.shutdown_grace_secs`. Surfaced here so a client
    /// command (`peppy node stop`) can size its request timeout to exceed the
    /// daemon's grace + reap window.
    pub shutdown_grace_secs: u64,
    /// The namespace this daemon generation resolved at startup (`local` when
    /// logged out, else the workspace id). A CLI control session reads it so
    /// it opens its session under exactly the daemon's namespace; it is
    /// written before the control socket binds, so a reader never sees a
    /// half-set generation. Typed, so an invalid value fails once at
    /// [`read_from`](Self::read_from) instead of each reader re-parsing it
    /// with its own failure policy.
    pub namespace: config::namespace::Namespace,
    pub router_ownership: RouterOwnership,
    /// `Some(secs)` when this daemon generation exposes identity control
    /// (managed and external Zenoh modes). Router ownership is reported by the
    /// separate typed field above and must never be inferred from this timeout.
    pub federation_connect_timeout_secs: Option<u64>,
    /// Whether this daemon generation started with `PEPPY_API_KEY` in its
    /// service environment.
    pub service_pat_active: bool,
    /// Whether this generation adopted a `zenoh.external` router. Pinned
    /// managed routers are also operator-managed, so ownership alone cannot
    /// select the correct CLI instructions.
    pub router_external: bool,
    /// Populated immediately before a normal managed-router logout writes its
    /// durable intent. It is deliberately absent for external/unmanaged
    /// routers and before the first logout transaction.
    #[serde(default)]
    pub managed_router_process: Option<ManagedRouterProcess>,
    /// Pre-spawn generation fence. Unlike `managed_router_process`, this is
    /// present before `Command::spawn` and remains usable if an apply future is
    /// cancelled during PMI's readiness wait.
    #[serde(default)]
    pub managed_router_launch: Option<ManagedRouterLaunch>,
}

impl DaemonState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        core_node_name: impl Into<String>,
        messaging_host: impl Into<String>,
        messaging_port: u16,
        git_hash: impl Into<String>,
        shutdown_grace_secs: u64,
        namespace: config::namespace::Namespace,
        router_ownership: RouterOwnership,
        federation_connect_timeout_secs: Option<u64>,
    ) -> Self {
        Self {
            core_node_name: core_node_name.into(),
            daemon_pid: Some(std::process::id()),
            messaging_host: messaging_host.into(),
            messaging_port,
            git_hash: git_hash.into(),
            shutdown_grace_secs,
            namespace,
            router_ownership,
            federation_connect_timeout_secs,
            service_pat_active: false,
            router_external: false,
            managed_router_process: None,
            managed_router_launch: None,
        }
    }

    /// Records the daemon process's own startup view of PEPPY_API_KEY.
    pub fn with_service_pat_active(mut self, active: bool) -> Self {
        self.service_pat_active = active;
        self
    }

    pub fn with_router_external(mut self, external: bool) -> Self {
        self.router_external = external;
        self
    }

    pub(crate) fn with_managed_router_process(mut self, process: ManagedRouterProcess) -> Self {
        self.managed_router_process = Some(process);
        self
    }

    pub(crate) fn with_managed_router_launch(mut self, launch: ManagedRouterLaunch) -> Self {
        self.managed_router_launch = Some(launch);
        self
    }

    /// Returns the path where the daemon state file is stored: the data
    /// root's `daemon_state.json5`.
    pub(crate) fn state_file_path() -> PathBuf {
        Self::state_file_in(PeppyDirs::default().root())
    }

    /// Returns the path where the daemon state file would be stored in the given directory.
    pub fn state_file_in(dir: impl AsRef<Path>) -> PathBuf {
        dir.as_ref().join(DAEMON_STATE_FILENAME)
    }

    pub fn write_to(path: &Path, state: &DaemonState) -> Result<(), io::Error> {
        let content =
            json5_pretty::to_string_pretty(state).map_err(|e| io::Error::other(e.to_string()))?;
        daemon_config::atomic_write::publish_atomic(path, |temporary| {
            let mut file = OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(temporary)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()
        })?;
        // The process/launch fence must survive a host crash before a later
        // durable logout intent can rely on it. Syncing the renamed file alone
        // does not make its parent-directory entry durable.
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            File::open(parent)?.sync_all()?;
        }
        Ok(())
    }

    /// Reads the daemon state from the data root's `daemon_state.json5`. The
    /// serve singleton lock guarantees at most one daemon writes it.
    pub fn read() -> Result<Self, io::Error> {
        Self::read_from(&Self::state_file_path())
    }

    pub fn read_from(path: &Path) -> Result<Self, io::Error> {
        let content = fs::read_to_string(path)?;
        serde_json5::from_str(&content).map_err(|e| io::Error::other(e.to_string()))
    }

    /// Whether the daemon that wrote this state still appears to be running, by
    /// probing its recorded pid. A state file outlives a crashed daemon (it is
    /// left on disk), so a successful [`read`](Self::read) is not by itself proof
    /// of liveness; a caller that needs "is a daemon actually up" must check this.
    pub fn is_running(&self) -> bool {
        self.daemon_pid.is_some_and(Self::pid_looks_alive)
    }

    #[cfg(unix)]
    fn pid_looks_alive(pid: u32) -> bool {
        use rustix::io::Errno;
        use rustix::process::{Pid, test_kill_process};

        // A daemon pid is always a positive, in-range process id; anything that
        // cannot be one names no live process we could probe.
        let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
            return false;
        };

        // `test_kill_process` is `kill(pid, 0)`: it sends no signal, just probes
        // existence and signalling permission.
        match test_kill_process(pid) {
            Ok(()) => true,
            // No such process.
            Err(Errno::SRCH) => false,
            // EPERM means the process exists but we may not signal it; any other
            // probe error is treated as alive so a transient failure never
            // discards a state file that may belong to a running daemon.
            Err(_) => true,
        }
    }

    #[cfg(not(unix))]
    fn pid_looks_alive(_pid: u32) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_round_trips_state() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("daemon_state.json5");
        let original = DaemonState {
            core_node_name: "node-42".to_string(),
            daemon_pid: Some(42),
            messaging_host: "router.internal".to_string(),
            messaging_port: 7447,
            git_hash: "test".to_string(),
            shutdown_grace_secs: 5,
            namespace: config::namespace::Namespace::local(),
            router_ownership: RouterOwnership::PeppyManaged,
            federation_connect_timeout_secs: Some(30),
            service_pat_active: false,
            router_external: false,
            managed_router_process: None,
            managed_router_launch: None,
        };
        DaemonState::write_to(&path, &original).expect("write");

        let read = DaemonState::read_from(&path).expect("read");
        assert_eq!(read.core_node_name, "node-42");
        assert_eq!(read.daemon_pid, Some(42));
        assert_eq!(read.messaging_host, "router.internal");
        assert_eq!(read.messaging_port, 7447);
        assert_eq!(read.git_hash, "test");
        assert_eq!(read.shutdown_grace_secs, 5);
        assert_eq!(read.namespace, config::namespace::Namespace::local());
        assert_eq!(read.router_ownership, RouterOwnership::PeppyManaged);
        assert_eq!(read.federation_connect_timeout_secs, Some(30));
        assert!(!read.service_pat_active);
    }

    #[test]
    fn state_file_with_unknown_fields_still_parses() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("daemon_state.json5");
        std::fs::write(
            &path,
            r#"{
                "core_node_name": "core",
                "daemon_pid": null,
                "messaging_host": "127.0.0.1",
                "messaging_port": 7447,
                "git_hash": "test",
                "shutdown_grace_secs": 5,
                "namespace": "local",
                "router_ownership": "unmanaged",
                "federation_connect_timeout_secs": null,
                "service_pat_active": false,
                "router_external": false,
                "managed_router_process": null,
                "managed_router_launch": null,
                "written_at_ms": 1234
            }"#,
        )
        .expect("write file with unknown field");

        let read = DaemonState::read_from(&path).expect("read");
        assert_eq!(read.core_node_name, "core");
        assert_eq!(read.daemon_pid, None);
        assert_eq!(read.messaging_host, "127.0.0.1");
        assert_eq!(read.messaging_port, 7447);
        assert!(!read.service_pat_active);
    }

    /// The namespace is parsed once at the read boundary: an invalid value
    /// fails `read_from` instead of every reader re-parsing it with its own
    /// failure policy.
    #[test]
    fn an_invalid_namespace_fails_the_read() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("daemon_state.json5");
        std::fs::write(
            &path,
            r#"{
                "core_node_name": "core",
                "daemon_pid": null,
                "messaging_host": "127.0.0.1",
                "messaging_port": 7447,
                "git_hash": "test",
                "shutdown_grace_secs": 5,
                "namespace": "**",
                "router_ownership": "unmanaged",
                "federation_connect_timeout_secs": null,
                "service_pat_active": false,
                "router_external": false,
                "managed_router_process": null,
                "managed_router_launch": null
            }"#,
        )
        .expect("write file with an invalid namespace");

        assert!(DaemonState::read_from(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn pid_looks_alive_for_the_current_process() {
        assert!(DaemonState::pid_looks_alive(std::process::id()));
    }
}
