use std::fs::{self};
use std::io;
use std::path::{Path, PathBuf};

use config::consts::{AppEnv, app_env};
use serde::{Deserialize, Serialize};

pub const DAEMON_STATE_FILE_ENV: &str = "PEPPY_DAEMON_STATE_FILE";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonState {
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
