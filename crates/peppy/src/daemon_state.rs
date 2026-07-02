use std::path::{Path, PathBuf};

use daemon_config::consts::PeppyDirs;
use serde::{Deserialize, Serialize};
use std::fs::{self};
use std::io;

const DAEMON_STATE_FILENAME: &str = "daemon_state.json5";

/// Persistent state for the peppy daemon.
///
/// This struct is serialized to JSON5 and stored on disk to track daemon state
/// across restarts. The state file location is determined by the `PEPPY_DAEMON_STATE_FILE`
/// environment variable, or defaults to `~/.peppy/$DAEMON_STATE_FILENAME` in production
/// and a temp directory in development.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DaemonState {
    /// The name of the node currently acting as the core node.
    pub core_node_name: String,
    pub daemon_pid: Option<u32>,
    /// The port the messaging router is listening on.
    #[serde(default = "default_messaging_port")]
    pub messaging_port: u16,
    /// The git hash of the peppy binary at compile time.
    #[serde(default)]
    pub git_hash: String,
    /// Cooperative-shutdown grace period the daemon resolved from
    /// `peppy_config.lifecycle.shutdown_grace_secs`. Surfaced here so a client
    /// command (`peppy node stop`) can size its request timeout to exceed the
    /// daemon's grace + reap window. Defaulted on read so a state file written
    /// by a daemon predating this field still parses.
    #[serde(default = "default_shutdown_grace_secs")]
    pub shutdown_grace_secs: u64,
    /// Wall-clock time (epoch milliseconds) captured when this state was built,
    /// which the daemon does immediately before writing it. Used to pick the
    /// freshest file when several exist and none has a live pid: it reflects
    /// logical write order more faithfully than filesystem mtime (which is
    /// coarse and can be rewritten by a copy or `touch`) and, living in the
    /// value, makes the selection deterministic and unit-testable. Defaulted on
    /// read so a state file written before this field still parses.
    #[serde(default)]
    pub written_at_ms: u64,
    /// The organization namespace this daemon generation resolved at startup
    /// (`"local"` when logged out, else the org id). A CLI control session reads
    /// it so it opens its session under exactly the daemon's namespace; it is
    /// written before the control socket binds, so a reader never sees a half-set
    /// generation. Defaulted to `"local"` on read so a state file written before
    /// this field still parses.
    #[serde(default = "default_organization_namespace")]
    pub organization_namespace: String,
}

fn default_organization_namespace() -> String {
    config::org::LOCAL_NAMESPACE.to_string()
}

fn default_messaging_port() -> u16 {
    config::consts::DEFAULT_MESSAGING_PORT
}

fn default_shutdown_grace_secs() -> u64 {
    config::peppy_config::DEFAULT_SHUTDOWN_GRACE_SECS
}

/// Current wall-clock time as epoch milliseconds, or 0 if the clock is set
/// before the Unix epoch (which the ranking treats as the oldest possible).
fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

impl DaemonState {
    pub(crate) fn new(
        core_node_name: impl Into<String>,
        messaging_port: u16,
        git_hash: impl Into<String>,
        shutdown_grace_secs: u64,
        organization_namespace: impl Into<String>,
    ) -> Self {
        Self {
            core_node_name: core_node_name.into(),
            daemon_pid: Some(std::process::id()),
            messaging_port,
            git_hash: git_hash.into(),
            shutdown_grace_secs,
            written_at_ms: now_epoch_ms(),
            organization_namespace: organization_namespace.into(),
        }
    }

    /// Returns the path where the daemon state file will be stored.
    ///
    /// If the `PEPPY_DAEMON_STATE_FILE` environment variable is set, returns that path.
    /// Otherwise, returns `peppy_data_dir()/daemon_state.json5`.
    pub(crate) fn state_file_path() -> PathBuf {
        Self::env_state_file_path().unwrap_or_else(Self::default_state_file_path)
    }

    /// Returns the path where the daemon state file would be stored in the given directory.
    #[cfg(feature = "test-support")]
    pub(crate) fn state_file_in(dir: impl AsRef<Path>) -> PathBuf {
        dir.as_ref().join(DAEMON_STATE_FILENAME)
    }

    pub(crate) fn write(&self) -> Result<PathBuf, io::Error> {
        let path = Self::state_file_path();
        Self::write_to(&path, self)?;
        Ok(path)
    }

    pub(crate) fn write_to(path: &Path, state: &DaemonState) -> Result<(), io::Error> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content =
            json5_pretty::to_string_pretty(state).map_err(|e| io::Error::other(e.to_string()))?;
        fs::write(path, content)
    }

    pub(crate) fn read() -> Result<Self, io::Error> {
        if let Some(path) = Self::env_state_file_path() {
            return Self::read_from(&path);
        }

        let mut states: Vec<(PathBuf, DaemonState)> = Vec::new();
        let mut last_err: Option<io::Error> = None;
        for path in Self::candidate_state_file_paths() {
            match Self::read_from(&path) {
                Ok(state) => states.push((path, state)),
                Err(err) => {
                    last_err = Some(err);
                }
            }
        }

        match states.len() {
            0 => Err(last_err.unwrap_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Daemon state file not found")
            })),
            1 => Ok(states.into_iter().next().expect("states length checked").1),
            _ => Self::select_best_state(states)?
                .map(|(_, state)| state)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "Daemon state file not found")
                }),
        }
    }

    pub(crate) fn read_from(path: &Path) -> Result<Self, io::Error> {
        let content = fs::read_to_string(path)?;
        serde_json5::from_str(&content).map_err(|e| io::Error::other(e.to_string()))
    }

    fn env_state_file_path() -> Option<PathBuf> {
        daemon_config::consts::non_empty_env_path(std::env::var_os(
            daemon_config::consts::DAEMON_STATE_FILE_ENV,
        ))
    }

    fn default_state_file_path() -> PathBuf {
        PeppyDirs::default().root().join(DAEMON_STATE_FILENAME)
    }

    fn candidate_state_file_paths() -> Vec<PathBuf> {
        let root = PeppyDirs::default().root().to_path_buf();
        let mut paths = vec![Self::default_state_file_path()];

        if let Ok(entries) = fs::read_dir(&root) {
            let mut dirs: Vec<PathBuf> = entries
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| match entry.file_type() {
                    Ok(ft) if ft.is_dir() => Some(entry.path()),
                    _ => None,
                })
                .collect();
            dirs.sort();

            for dir in dirs {
                paths.push(dir.join(DAEMON_STATE_FILENAME));
            }
        }
        paths
    }

    fn select_best_state(
        states: Vec<(PathBuf, DaemonState)>,
    ) -> Result<Option<(PathBuf, DaemonState)>, io::Error> {
        Self::rank_states(states, Self::pid_looks_alive)
    }

    /// Whether the daemon that wrote this state still appears to be running, by
    /// probing its recorded pid. A state file outlives a crashed daemon (it is
    /// left on disk), so a successful [`read`](Self::read) is not by itself proof
    /// of liveness; a caller that needs "is a daemon actually up" must check this.
    pub(crate) fn is_running(&self) -> bool {
        self.daemon_pid.is_some_and(Self::pid_looks_alive)
    }

    /// Picks the state file that best represents the live daemon, given a
    /// liveness predicate (injected so tests can stub it without spawning real
    /// processes):
    /// - if exactly one candidate has a live pid, it wins;
    /// - if several do, the situation is ambiguous and an error;
    /// - otherwise the most recently written candidate wins, ranked on the
    ///   serialized `written_at_ms` with the path as a deterministic tie-break.
    ///
    /// Pure: it reads no filesystem metadata and no clock, so the same inputs
    /// always select the same state.
    fn rank_states(
        states: Vec<(PathBuf, DaemonState)>,
        is_alive: impl Fn(u32) -> bool,
    ) -> Result<Option<(PathBuf, DaemonState)>, io::Error> {
        let mut running: Vec<(PathBuf, DaemonState)> = states
            .iter()
            .filter(|(_, state)| state.daemon_pid.is_some_and(&is_alive))
            .cloned()
            .collect();

        match running.len() {
            0 => {}
            1 => return Ok(running.pop()),
            _ => {
                return Err(io::Error::other(format!(
                    "Multiple running peppy daemons detected. Set {} to select one.",
                    daemon_config::consts::DAEMON_STATE_FILE_ENV
                )));
            }
        }

        Ok(states.into_iter().max_by(|(a_path, a), (b_path, b)| {
            a.written_at_ms
                .cmp(&b.written_at_ms)
                .then_with(|| a_path.cmp(b_path))
        }))
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

    /// Builds a state with a given pid and write time; the other fields are
    /// irrelevant to selection, so they get fixed placeholders.
    fn state(pid: Option<u32>, written_at_ms: u64) -> DaemonState {
        DaemonState {
            core_node_name: format!("node-{}", pid.unwrap_or(0)),
            daemon_pid: pid,
            messaging_port: 7447,
            git_hash: "test".to_string(),
            shutdown_grace_secs: 5,
            written_at_ms,
            organization_namespace: "local".to_string(),
        }
    }

    fn path(name: &str) -> PathBuf {
        PathBuf::from(name)
    }

    #[test]
    fn live_pid_wins_over_a_newer_but_dead_state() {
        let states = vec![
            (path("a.json5"), state(Some(10), 100)), // alive, older write
            (path("b.json5"), state(Some(20), 999)), // dead, newer write
        ];
        // Only pid 10 is alive.
        let chosen = DaemonState::rank_states(states, |pid| pid == 10)
            .expect("ranking succeeds")
            .expect("a candidate is chosen");
        assert_eq!(chosen.0, path("a.json5"));
    }

    #[test]
    fn single_running_daemon_is_chosen() {
        let states = vec![
            (path("a.json5"), state(Some(10), 100)),
            (path("b.json5"), state(Some(20), 200)),
        ];
        let chosen = DaemonState::rank_states(states, |pid| pid == 20)
            .expect("ranking succeeds")
            .expect("a candidate is chosen");
        assert_eq!(chosen.0, path("b.json5"));
    }

    #[test]
    fn multiple_running_daemons_is_an_error() {
        let states = vec![
            (path("a.json5"), state(Some(10), 100)),
            (path("b.json5"), state(Some(20), 200)),
        ];
        // Both pids report alive.
        let err = DaemonState::rank_states(states, |_| true).unwrap_err();
        assert!(
            err.to_string().contains("Multiple running peppy daemons"),
            "got: {err}"
        );
    }

    #[test]
    fn with_no_live_daemon_the_freshest_write_wins() {
        let states = vec![
            (path("a.json5"), state(Some(10), 100)),
            (path("b.json5"), state(Some(20), 300)),
            (path("c.json5"), state(None, 200)),
        ];
        let chosen = DaemonState::rank_states(states, |_| false)
            .expect("ranking succeeds")
            .expect("a candidate is chosen");
        assert_eq!(chosen.0, path("b.json5"), "highest written_at_ms wins");
    }

    #[test]
    fn equal_write_times_break_ties_by_path_deterministically() {
        let states = vec![
            (path("z.json5"), state(None, 500)),
            (path("a.json5"), state(None, 500)),
        ];
        let chosen = DaemonState::rank_states(states, |_| false)
            .expect("ranking succeeds")
            .expect("a candidate is chosen");
        // Tie on write time falls back to the greatest path, deterministically.
        assert_eq!(chosen.0, path("z.json5"));
    }

    #[test]
    fn empty_candidate_set_selects_nothing() {
        let chosen = DaemonState::rank_states(Vec::new(), |_| true).expect("ranking succeeds");
        assert!(chosen.is_none());
    }

    #[test]
    fn write_then_read_preserves_written_at_ms() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("daemon_state.json5");
        let mut original = state(Some(42), 0);
        original.written_at_ms = 1_234_567;
        DaemonState::write_to(&path, &original).expect("write");

        let read = DaemonState::read_from(&path).expect("read");
        assert_eq!(read.written_at_ms, 1_234_567);
    }

    #[test]
    fn state_file_without_written_at_ms_defaults_to_zero() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("daemon_state.json5");
        // A file written before the field existed.
        std::fs::write(&path, r#"{ "core_node_name": "old", "daemon_pid": null }"#)
            .expect("write legacy file");

        let read = DaemonState::read_from(&path).expect("read");
        assert_eq!(read.written_at_ms, 0);
    }

    #[cfg(unix)]
    #[test]
    fn pid_looks_alive_for_the_current_process() {
        assert!(DaemonState::pid_looks_alive(std::process::id()));
    }
}
