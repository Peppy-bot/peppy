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

/// Persistent state for the peppy daemon.
///
/// This struct is serialized to JSON and stored on disk to track daemon state
/// across restarts. The state file location is determined by the `PEPPY_DAEMON_STATE_FILE`
/// environment variable, or defaults to `/var/run/peppy/daemon_state.json` in production
/// and a temp directory in development.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonState {
    /// The name of the node currently acting as the master node.
    pub master_node_name: String,
}

impl DaemonState {
    pub fn new(master_node_name: impl Into<String>) -> Self {
        Self {
            master_node_name: master_node_name.into(),
        }
    }

    pub fn write(&self) -> Result<PathBuf, io::Error> {
        let path = Self::state_file_path();
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
        let path = Self::state_file_path();
        Self::read_from(&path)
    }

    pub fn read_from(path: &Path) -> Result<Self, io::Error> {
        let content = fs::read_to_string(path)?;
        serde_json::from_str(&content).map_err(|e| io::Error::other(e.to_string()))
    }

    pub fn remove() -> Result<(), io::Error> {
        let path = Self::state_file_path();
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn state_file_path() -> PathBuf {
        if let Some(value) = std::env::var_os(DAEMON_STATE_FILE_ENV) {
            return PathBuf::from(value);
        }

        match app_env() {
            AppEnv::Prod => PathBuf::from("/var/run/peppy/daemon_state.json"),
            AppEnv::Dev => std::env::temp_dir().join("peppy").join("daemon_state.json"),
        }
    }
}
