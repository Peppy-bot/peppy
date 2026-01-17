pub const MASTER_NODE_TOPIC_NAME: &str = "command";
pub const NODE_CONFIG_FILE: &str = "peppy.json5";
pub const RUNTIME_CONFIG_VAR_NAME: &str = "PEPPY_RUNTIME_CONFIG";
/// The standard output directory for generated peppygen libraries relative to node_dir.
pub const PEPPYGEN_OUTPUT_PATH: &str = ".peppy/libs/peppygen";
pub const PEPPYLIB_OUTPUT_PATH: &str = ".peppy/libs/peppygen/crates/peppylib";
// 7447 is the default port but we avoid using it to avoid conflicts with other services using Zenoh
pub const DEFAULT_ZENOH_HOST: &str = "127.0.0.1";
// 7447 is the default port but we avoid using it to avoid conflicts with other services using Zenoh
pub const DEFAULT_ZENOH_PORT: u16 = 7448;
pub const DAEMON_STATE_FILE_ENV: &str = "PEPPY_DAEMON_STATE_FILE";

pub const ALLOWED_CONFIG_CHARS: &str =
    "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-";

// Application runtime environment (dev/prod) tracked internally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEnv {
    Dev,
    Prod,
}

use std::sync::OnceLock;

static APP_ENV: OnceLock<AppEnv> = OnceLock::new();

/// Sets the application environment once. Subsequent calls are ignored.
pub fn set_app_env(env: AppEnv) {
    let _ = APP_ENV.set(env);
}

/// Returns the current application environment, defaulting to Dev.
pub fn app_env() -> AppEnv {
    *APP_ENV.get_or_init(|| AppEnv::Dev)
}

/// Returns the base peppy data directory.
/// In production: ~/.peppy
/// In development: /tmp/.peppy
pub fn peppy_data_dir() -> std::path::PathBuf {
    match app_env() {
        AppEnv::Prod => {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(std::path::PathBuf::from);
            home.unwrap_or_else(std::env::temp_dir).join(".peppy")
        }
        AppEnv::Dev => std::env::temp_dir().join(".peppy"),
    }
}

/// Returns the nodes cache directory.
/// In production: ~/.peppy/nodes
/// In development: /tmp/.peppy/nodes
pub fn nodes_cache_dir() -> std::path::PathBuf {
    peppy_data_dir().join("nodes")
}

/// Returns the add logs cache directory.
/// In production: ~/.peppy/logs/add
/// In development: /tmp/.peppy/logs/add
pub fn logs_dir_add() -> std::path::PathBuf {
    peppy_data_dir().join("logs").join("add")
}

/// Returns the start logs cache directory.
/// In production: ~/.peppy/logs/start
/// In development: /tmp/.peppy/logs/start
pub fn logs_dir_start() -> std::path::PathBuf {
    peppy_data_dir().join("logs").join("start")
}

/// Returns the runtime config file path.
pub fn runtime_config_path() -> std::path::PathBuf {
    peppy_data_dir().join("runtime/runtime_config.json")
}
