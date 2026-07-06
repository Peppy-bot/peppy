//! Environment-variable handling used by the node command handlers:
//! validation against a blocklist of injection-prone keys, and the
//! runtime/build env injection shared by `node_add`, `node_build`, and
//! `node_run`.

use crate::{Error, Result};
use config::node::PeppygenLanguage;
use core_node_api::FORBIDDEN_ENV_KEYS;
use std::sync::OnceLock;

pub(crate) const PEPPY_APPTAINER_BIN_ENV_VAR: &str = "PEPPY_APPTAINER_BIN";
pub(crate) const PEPPY_NODE_NAME_ENV_VAR: &str = "PEPPY_NODE_NAME";
pub(crate) const PEPPY_NODE_TAG_ENV_VAR: &str = "PEPPY_NODE_TAG";

/// Validates environment variables, rejecting any that are in the forbidden list.
/// Returns an error if any forbidden env var is found.
pub(crate) fn validate_goal_env_vars(
    env_vars: &[(String, String)],
) -> Result<Vec<(String, String)>> {
    let mut result = Vec::with_capacity(env_vars.len());
    for (key, value) in env_vars {
        let trimmed = key.trim();
        if trimmed.is_empty() {
            return Err(Error::InvalidEnvVar("empty key".to_string()));
        }
        if trimmed.contains('=') {
            return Err(Error::InvalidEnvVar(format!(
                "key '{trimmed}' contains '='"
            )));
        }
        if trimmed.as_bytes().contains(&0) {
            return Err(Error::InvalidEnvVar(format!(
                "key '{trimmed}' contains a NUL byte"
            )));
        }
        if value.as_bytes().contains(&0) {
            return Err(Error::InvalidEnvVar(format!(
                "value for key '{trimmed}' contains a NUL byte"
            )));
        }
        let normalized = trimmed.to_ascii_uppercase();
        if FORBIDDEN_ENV_KEYS.contains(&normalized.as_str()) {
            return Err(Error::ForbiddenEnvVar(normalized));
        }
        result.push((trimmed.to_string(), value.clone()));
    }
    Ok(result)
}

static SCCACHE_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Checks whether `sccache` is available on the system PATH.
/// The result is cached for the lifetime of the process.
pub(crate) fn is_sccache_available() -> bool {
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

/// Injects `RUSTC_WRAPPER=sccache` for Rust nodes when sccache is available
/// on the system PATH and the user has not already provided a `RUSTC_WRAPPER`
/// value.
///
/// User-provided values are never overwritten.
///
/// Returns `true` if `RUSTC_WRAPPER=sccache` was injected.
pub(crate) fn inject_rust_build_env(
    env_vars: &mut Vec<(String, String)>,
    language: PeppygenLanguage,
) -> bool {
    if language != PeppygenLanguage::Rust {
        return false;
    }
    let sccache_injected = !has_env_key(env_vars, "RUSTC_WRAPPER") && is_sccache_available();
    if sccache_injected {
        env_vars.push(("RUSTC_WRAPPER".to_string(), "sccache".to_string()));
    }
    sccache_injected
}

fn has_env_key(env_vars: &[(String, String)], key: &str) -> bool {
    env_vars.iter().any(|(k, _)| k == key)
}

fn push_env_if_missing(env_vars: &mut Vec<(String, String)>, key: &str, value: String) {
    if !has_env_key(env_vars, key) {
        env_vars.push((key.to_string(), value));
    }
}

fn resolve_apptainer_bin() -> String {
    if let Ok(value) = std::env::var(PEPPY_APPTAINER_BIN_ENV_VAR) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    if let Ok(current_exe) = std::env::current_exe()
        && let Some(exe_dir) = current_exe.parent()
    {
        let bundled_apptainer = exe_dir.join("apptainer").join("bin").join("apptainer");
        if bundled_apptainer.is_file() {
            return bundled_apptainer.to_string_lossy().into_owned();
        }
    }

    "apptainer".to_string()
}

pub(crate) fn inject_node_runtime_env(
    env_vars: &mut Vec<(String, String)>,
    node_name: &str,
    node_tag: &str,
) {
    push_env_if_missing(
        env_vars,
        PEPPY_APPTAINER_BIN_ENV_VAR,
        resolve_apptainer_bin(),
    );
    push_env_if_missing(env_vars, PEPPY_NODE_NAME_ENV_VAR, node_name.to_string());
    push_env_if_missing(env_vars, PEPPY_NODE_TAG_ENV_VAR, node_tag.to_string());
}
