use config::consts::{AppEnv, app_env};
use peppylib::MessengerHandle;
use pmi::Messenger;
use serde::{Deserialize, Serialize};
use std::fs::{self};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell};

const DEFAULT_ZENOH_HOST: &str = "127.0.0.1";

pub struct AppContext {
    pub root_dir: PathBuf,
    messenger_handle: OnceCell<MessengerHandle>,
}

impl AppContext {
    pub fn new(root_dir: impl AsRef<Path>) -> Self {
        Self {
            root_dir: PathBuf::from(root_dir.as_ref()),
            messenger_handle: OnceCell::new(),
        }
    }

    /// Creates an AppContext with a pre-initialized messenger handle.
    /// This is useful for testing with a shared mock messenger.
    pub fn with_messenger(root_dir: impl AsRef<Path>, messenger: Arc<Mutex<Messenger>>) -> Self {
        let ctx = Self::new(root_dir);
        let _ = ctx
            .messenger_handle
            .set(MessengerHandle::from_shared(messenger));
        ctx
    }

    pub async fn connect(&self) -> crate::error::Result<()> {
        self.messenger_handle
            .get_or_try_init(|| async {
                MessengerHandle::from_host_port(
                    DEFAULT_ZENOH_HOST,
                    config::consts::DEFAULT_ZENOH_PORT,
                )
                .await
            })
            .await?;
        Ok(())
    }

    pub fn messenger_handle(&self) -> Option<&MessengerHandle> {
        self.messenger_handle.get()
    }
}

impl Default for AppContext {
    fn default() -> Self {
        let root_dir = std::env::current_dir().expect("Failed to get current directory");
        Self::new(root_dir)
    }
}

pub const DAEMON_STATE_FILE_ENV: &str = "PEPPY_DAEMON_STATE_FILE";
const DAEMON_STATE_FILENAME: &str = "daemon_state.json";

/// Persistent state for the peppy daemon.
///
/// This struct is serialized to JSON and stored on disk to track daemon state
/// across restarts. The state file location is determined by the `PEPPY_DAEMON_STATE_FILE`
/// environment variable, or defaults to `~/.peppy/<master_node_name>/daemon_state.json` in production
/// and a temp directory in development.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonState {
    /// The name of the node currently acting as the master node.
    pub master_node_name: String,
    #[serde(default)]
    pub daemon_pid: Option<u32>,
}

impl DaemonState {
    pub fn new(master_node_name: impl Into<String>) -> Self {
        Self {
            master_node_name: master_node_name.into(),
            daemon_pid: Some(std::process::id()),
        }
    }

    pub fn write(&self) -> Result<PathBuf, io::Error> {
        if let Some(path) = Self::env_state_file_path() {
            Self::write_to(&path, self)?;
            return Ok(path);
        }

        let path = Self::state_file_path_for_master_node(&self.master_node_name);
        Self::write_to(&path, self)?;
        Ok(path)
    }

    pub fn write_to(path: &Path, state: &DaemonState) -> Result<(), io::Error> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content =
            serde_json::to_string_pretty(state).map_err(|e| io::Error::other(e.to_string()))?;
        fs::write(path, content)
    }

    pub fn read() -> Result<Self, io::Error> {
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

    pub fn read_from(path: &Path) -> Result<Self, io::Error> {
        let content = fs::read_to_string(path)?;
        serde_json::from_str(&content).map_err(|e| io::Error::other(e.to_string()))
    }

    pub fn remove() -> Result<(), io::Error> {
        if let Some(path) = Self::env_state_file_path() {
            return match fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            };
        }

        let mut states: Vec<(PathBuf, DaemonState)> = Vec::new();
        for path in Self::candidate_state_file_paths() {
            if let Ok(state) = Self::read_from(&path) {
                states.push((path, state));
            }
        }

        let Some((path, _)) = Self::select_best_state(states)? else {
            return Ok(());
        };

        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }

        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir(parent);
        }

        Ok(())
    }

    fn env_state_file_path() -> Option<PathBuf> {
        std::env::var_os(DAEMON_STATE_FILE_ENV).map(PathBuf::from)
    }

    fn state_root_dir() -> PathBuf {
        match app_env() {
            AppEnv::Prod => Self::prod_state_root_dir(),
            AppEnv::Dev => Self::dev_state_root_dir(),
        }
    }

    fn prod_state_root_dir() -> PathBuf {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);

        home.unwrap_or_else(std::env::temp_dir).join(".peppy")
    }

    fn dev_state_root_dir() -> PathBuf {
        std::env::temp_dir().join("peppy")
    }

    pub fn state_file_path_for_master_node(master_node_name: &str) -> PathBuf {
        Self::state_root_dir()
            .join(master_node_name)
            .join(DAEMON_STATE_FILENAME)
    }

    fn candidate_state_file_paths() -> Vec<PathBuf> {
        let root = Self::state_root_dir();
        let mut paths = Vec::new();

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
            .filter(|(_, state)| {
                state
                    .daemon_pid
                    .map_or(false, |pid| Self::pid_looks_alive(pid))
            })
            .cloned()
            .collect();

        match running.len() {
            0 => {}
            1 => return Ok(running.pop()),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "Multiple running peppy daemons detected. Set {} to select one.",
                        DAEMON_STATE_FILE_ENV
                    ),
                ));
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
