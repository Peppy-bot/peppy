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
use config::node::{NodeConfig, PeppygenLanguage};
use std::sync::OnceLock;

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
fn validate_goal_env_vars(env_vars: &[(String, String)]) -> Result<Vec<(String, String)>> {
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

static SCCACHE_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Checks whether `sccache` is available on the system PATH.
/// The result is cached for the lifetime of the process.
fn is_sccache_available() -> bool {
    *SCCACHE_AVAILABLE.get_or_init(|| {
        let available = std::process::Command::new("sccache")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if available {
            tracing::debug!("sccache detected, will set RUSTC_WRAPPER=sccache for Rust nodes");
        }
        available
    })
}

/// Injects Rust-specific build environment variables for Rust nodes:
/// - `CARGO_TARGET_DIR`: a stable, per-node target directory so compiled
///   artifacts survive across `peppy node add` runs while allowing parallel
///   builds of different nodes without cargo lock contention.
/// - `RUSTC_WRAPPER=sccache`: set when sccache is available on the system PATH
///   and the user has not already provided a `RUSTC_WRAPPER` value.
///
/// User-provided values for either variable are never overwritten.
///
/// Returns `true` if `RUSTC_WRAPPER=sccache` was injected.
fn inject_rust_build_env(
    env_vars: &mut Vec<(String, String)>,
    language: PeppygenLanguage,
    node_name: &str,
    tag: &str,
) -> bool {
    if language != PeppygenLanguage::Rust {
        return false;
    }
    if !env_vars.iter().any(|(k, _)| k == "CARGO_TARGET_DIR") {
        env_vars.push((
            "CARGO_TARGET_DIR".to_string(),
            generator::rust_node_target_dir(node_name, tag)
                .to_string_lossy()
                .into_owned(),
        ));
    }
    let sccache_injected =
        !env_vars.iter().any(|(k, _)| k == "RUSTC_WRAPPER") && is_sccache_available();
    if sccache_injected {
        env_vars.push(("RUSTC_WRAPPER".to_string(), "sccache".to_string()));
    }
    sccache_injected
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_rust_build_env_skips_python_nodes() {
        let mut env_vars = vec![("FOO".to_string(), "bar".to_string())];
        inject_rust_build_env(&mut env_vars, PeppygenLanguage::Python, "mynode", "0.1.0");
        assert!(
            !env_vars.iter().any(|(k, _)| k == "RUSTC_WRAPPER"),
            "RUSTC_WRAPPER should not be set for Python nodes"
        );
        assert!(
            !env_vars.iter().any(|(k, _)| k == "CARGO_TARGET_DIR"),
            "CARGO_TARGET_DIR should not be set for Python nodes"
        );
    }

    #[test]
    fn inject_rust_build_env_sets_cargo_target_dir_for_rust() {
        let mut env_vars = vec![];
        inject_rust_build_env(&mut env_vars, PeppygenLanguage::Rust, "mynode", "0.1.0");
        let target_dir = env_vars
            .iter()
            .find(|(k, _)| k == "CARGO_TARGET_DIR")
            .expect("CARGO_TARGET_DIR should be set for Rust nodes");
        assert!(
            target_dir.1.contains("mynode_0.1.0"),
            "target dir should contain node name and tag, got: {}",
            target_dir.1
        );
    }

    #[test]
    fn inject_rust_build_env_different_nodes_get_different_target_dirs() {
        let mut env_a = vec![];
        inject_rust_build_env(&mut env_a, PeppygenLanguage::Rust, "node_a", "0.1.0");
        let mut env_b = vec![];
        inject_rust_build_env(&mut env_b, PeppygenLanguage::Rust, "node_b", "0.1.0");
        let dir_a = env_a.iter().find(|(k, _)| k == "CARGO_TARGET_DIR").unwrap();
        let dir_b = env_b.iter().find(|(k, _)| k == "CARGO_TARGET_DIR").unwrap();
        assert_ne!(
            dir_a.1, dir_b.1,
            "different nodes should have different target dirs"
        );
    }

    #[test]
    fn inject_rust_build_env_respects_user_overrides() {
        let mut env_vars = vec![
            ("RUSTC_WRAPPER".to_string(), "custom_wrapper".to_string()),
            ("CARGO_TARGET_DIR".to_string(), "/custom/target".to_string()),
        ];
        inject_rust_build_env(&mut env_vars, PeppygenLanguage::Rust, "mynode", "0.1.0");
        assert_eq!(
            env_vars
                .iter()
                .filter(|(k, _)| k == "RUSTC_WRAPPER")
                .count(),
            1,
            "should not duplicate RUSTC_WRAPPER"
        );
        assert_eq!(
            env_vars
                .iter()
                .find(|(k, _)| k == "RUSTC_WRAPPER")
                .unwrap()
                .1,
            "custom_wrapper",
            "should keep user-provided RUSTC_WRAPPER value"
        );
        assert_eq!(
            env_vars
                .iter()
                .find(|(k, _)| k == "CARGO_TARGET_DIR")
                .unwrap()
                .1,
            "/custom/target",
            "should keep user-provided CARGO_TARGET_DIR value"
        );
    }

    #[test]
    fn is_sccache_available_does_not_panic() {
        let _ = is_sccache_available();
    }
}
