use std::path::{Path, PathBuf};

use daemon_config::consts::PeppyDirs;
use serde::{Deserialize, Serialize};
use std::fs::{self};
use std::io;

const DAEMON_STATE_FILENAME: &str = "daemon_state.json5";

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
    /// The routing namespace this daemon generation resolved at startup from
    /// its enrollment (`local` when not enrolled, else the project id). A CLI
    /// control session reads it so it opens under exactly the daemon's
    /// namespace. The field is required and typed: missing or invalid
    /// persisted state fails at read.
    pub namespace: config::namespace::Namespace,
    /// The transport identity the managed router runs under: the enrollment's
    /// zenoh id when enrolled, a per-boot id otherwise. `None` for an
    /// operator-run external router (whose identity is the operator's) and for
    /// the mock engine. `peppy platform enroll` reads it, with `namespace`, to
    /// tell the rebuilt generation from the one it poked.
    pub router_id: Option<pmi::RouterId>,
    /// Whether this generation binds the federation control socket (a managed
    /// zenoh router). `false` for an operator-run external router or no
    /// router, so there is nothing to poke and no restart to warn about.
    /// `peppy platform enroll`/`unenroll` read this so they follow the RUNNING
    /// daemon's mode, not a config edited on disk after it started.
    pub federation_control: bool,
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
        router_id: Option<pmi::RouterId>,
        federation_control: bool,
    ) -> Self {
        Self {
            core_node_name: core_node_name.into(),
            daemon_pid: Some(std::process::id()),
            messaging_host: messaging_host.into(),
            messaging_port,
            git_hash: git_hash.into(),
            shutdown_grace_secs,
            namespace,
            router_id,
            federation_control,
        }
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
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content =
            json5_pretty::to_string_pretty(state).map_err(|e| io::Error::other(e.to_string()))?;
        fs::write(path, content)
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
            router_id: Some(pmi::RouterId::parse("7f3a9c1e").unwrap()),
            federation_control: true,
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
        assert_eq!(
            read.router_id,
            Some(pmi::RouterId::parse("7f3a9c1e").unwrap())
        );
        assert!(read.federation_control);
    }

    #[test]
    fn state_file_with_unknown_fields_still_parses() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("daemon_state.json5");
        std::fs::write(
            &path,
            r#"{
                "core_node_name": "old",
                "daemon_pid": null,
                "messaging_host": "127.0.0.1",
                "messaging_port": 7448,
                "git_hash": "old",
                "shutdown_grace_secs": 5,
                "namespace": "local",
                "router_id": null,
                "federation_control": false,
                "written_at_ms": 1234
            }"#,
        )
        .expect("write file with unknown field");

        let read = DaemonState::read_from(&path).expect("read");
        assert_eq!(read.core_node_name, "old");
        assert_eq!(read.daemon_pid, None);
        assert_eq!(read.messaging_host, "127.0.0.1");
        assert_eq!(read.messaging_port, 7448);
    }

    #[test]
    fn an_invalid_namespace_fails_the_read() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("daemon_state.json5");
        std::fs::write(
            &path,
            r#"{
                "core_node_name": "bad",
                "daemon_pid": null,
                "messaging_host": "127.0.0.1",
                "messaging_port": 7448,
                "git_hash": "test",
                "shutdown_grace_secs": 5,
                "namespace": "**",
                "router_id": null,
                "federation_control": false
            }"#,
        )
        .expect("write invalid state");

        assert!(DaemonState::read_from(&path).is_err());
    }

    /// A router id zenoh would refuse (a leading zero) fails the read the same
    /// way an invalid namespace does, rather than reaching a CLI comparison.
    #[test]
    fn an_invalid_router_id_fails_the_read() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("daemon_state.json5");
        std::fs::write(
            &path,
            r#"{
                "core_node_name": "bad",
                "daemon_pid": null,
                "messaging_host": "127.0.0.1",
                "messaging_port": 7448,
                "git_hash": "test",
                "shutdown_grace_secs": 5,
                "namespace": "local",
                "router_id": "0abc",
                "federation_control": true
            }"#,
        )
        .expect("write invalid state");

        assert!(DaemonState::read_from(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn pid_looks_alive_for_the_current_process() {
        assert!(DaemonState::pid_looks_alive(std::process::id()));
    }
}
