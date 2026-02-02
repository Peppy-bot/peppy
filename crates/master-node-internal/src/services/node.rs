mod add;
mod info;
mod init;
mod remove;
mod start;
mod stop;
mod sync;
mod templates;

use crate::encoding::NodeSource;
use crate::{Error, Result};
use config::node::NodeConfig;

/// Blocklist of dangerous env vars that could be used for code injection or process manipulation.
/// Used by both the daemon (to reject requests) and CLI (to filter before sending).
pub const FORBIDDEN_ENV_KEYS: [&str; 16] = [
    // Linux dynamic linker injection
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "LD_DEBUG",
    // macOS dynamic linker injection
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    // Shell injection vectors
    "BASH_ENV",
    "ENV",
    "CDPATH",
    "IFS",
    "SHELLOPTS",
    "BASHOPTS",
    "PS4",
    "PROMPT_COMMAND",
    "GLOBIGNORE",
];

/// Validates environment variables, rejecting any that are in the forbidden list.
/// Returns an error if any forbidden env var is found.
pub(self) fn validate_goal_env_vars(
    env_vars: &[(String, String)],
) -> Result<Vec<(String, String)>> {
    let mut result = Vec::with_capacity(env_vars.len());
    for (key, value) in env_vars {
        let normalized = key.trim().to_ascii_uppercase();
        if FORBIDDEN_ENV_KEYS.contains(&normalized.as_str()) {
            return Err(Error::ForbiddenEnvVar(normalized));
        }
        result.push((normalized, value.clone()));
    }
    Ok(result)
}

pub use add::listen_for_node_add;
pub use info::listen_for_node_info;
pub use init::listen_for_node_init;
pub use remove::listen_for_node_remove;
pub use start::listen_for_node_start;
pub use stop::listen_for_node_stop;
pub use sync::listen_for_node_sync;

pub(super) async fn resolve_node_config(
    source: NodeSource,
) -> std::result::Result<NodeConfig, String> {
    info::resolve_node_config(source).await
}
