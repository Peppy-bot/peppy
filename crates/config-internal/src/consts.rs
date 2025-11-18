pub const MASTER_NODE_TOPIC_NAMESPACE: &str = "master_node";
pub const MASTER_NODE_CMD_TOPIC_NAME: &str = "command";
pub const PEPPY_NODE_CONFIG_FILE: &str = "peppy.json5";
// 7447 is the default port but we avoid using it to avoid conflicts with other services using Zenoh
pub const DEFAULT_ZENOH_PORT: u16 = 7448;

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

/// Environment-aware root dir value.
pub fn logs_root_dir() -> &'static str {
    match app_env() {
        AppEnv::Dev => ".peppy/logs/",
        AppEnv::Prod => "/var/log/peppy/", // In prod the root dir is `/` on the system
    }
}
