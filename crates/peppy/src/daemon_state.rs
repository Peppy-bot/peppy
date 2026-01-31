use std::path::{Path, PathBuf};

use config::consts::peppy_data_dir;
use serde::{Deserialize, Serialize};
use std::fs::{self};
use std::io;

const DAEMON_STATE_FILENAME: &str = "daemon_state.json";

/// Persistent state for the peppy daemon.
///
/// This struct is serialized to JSON and stored on disk to track daemon state
/// across restarts. The state file location is determined by the `PEPPY_DAEMON_STATE_FILE`
/// environment variable, or defaults to `~/.peppy/$DAEMON_STATE_FILENAME` in production
/// and a temp directory in development.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DaemonState {
    /// The name of the node currently acting as the master node.
    pub master_node_name: String,
    pub daemon_pid: Option<u32>,
    /// The port the messaging router is listening on.
    #[serde(default = "default_messaging_port")]
    pub messaging_port: u16,
    /// The git hash of the peppy binary at compile time.
    #[serde(default)]
    pub git_hash: String,
}

fn default_messaging_port() -> u16 {
    config::consts::DEFAULT_MESSAGING_PORT
}

impl DaemonState {
    pub(crate) fn new(
        master_node_name: impl Into<String>,
        messaging_port: u16,
        git_hash: impl Into<String>,
    ) -> Self {
        Self {
            master_node_name: master_node_name.into(),
            daemon_pid: Some(std::process::id()),
            messaging_port,
            git_hash: git_hash.into(),
        }
    }

    /// Returns the path where the daemon state file will be stored.
    ///
    /// If the `PEPPY_DAEMON_STATE_FILE` environment variable is set, returns that path.
    /// Otherwise, returns `peppy_data_dir()/daemon_state.json`.
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
            serde_json::to_string_pretty(state).map_err(|e| io::Error::other(e.to_string()))?;
        fs::write(path, &content)?;
        println!("Daemon state written to: {}", path.display());
        Ok(())
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
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    last_err = Some(err);
                }
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
        serde_json::from_str(&content).map_err(|e| io::Error::other(e.to_string()))
    }

    fn env_state_file_path() -> Option<PathBuf> {
        std::env::var_os(config::consts::DAEMON_STATE_FILE_ENV).map(PathBuf::from)
    }

    fn default_state_file_path() -> PathBuf {
        peppy_data_dir().join(DAEMON_STATE_FILENAME)
    }

    fn candidate_state_file_paths() -> Vec<PathBuf> {
        let root = peppy_data_dir();
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
        if states.is_empty() {
            return Ok(None);
        }

        let mut running: Vec<(PathBuf, DaemonState)> = states
            .iter()
            .filter(|(_, state)| state.daemon_pid.is_some_and(Self::pid_looks_alive))
            .cloned()
            .collect();

        match running.len() {
            0 => {}
            1 => return Ok(running.pop()),
            _ => {
                return Err(io::Error::other(format!(
                    "Multiple running peppy daemons detected. Set {} to select one.",
                    config::consts::DAEMON_STATE_FILE_ENV
                )));
            }
        }

        let mut best: Option<(std::time::SystemTime, PathBuf, DaemonState)> = None;
        for (path, state) in states {
            let modified = fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            match &best {
                Some((best_modified, _, _)) if modified <= *best_modified => {}
                _ => best = Some((modified, path, state)),
            }
        }

        Ok(best.map(|(_, path, state)| (path, state)))
    }

    #[cfg(unix)]
    fn pid_looks_alive(pid: u32) -> bool {
        let pid = pid as libc::pid_t;
        unsafe {
            if libc::kill(pid, 0) == 0 {
                return true;
            }
        }

        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(code) if code == libc::ESRCH => false,
            // EPERM implies the process exists, but we don't have permission to signal it.
            Some(code) if code == libc::EPERM => true,
            _ => true,
        }
    }

    #[cfg(not(unix))]
    fn pid_looks_alive(_pid: u32) -> bool {
        false
    }
}
