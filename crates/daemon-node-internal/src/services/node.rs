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
use rand::RngExt;
use std::path::{Component, Path};
use std::sync::OnceLock;
use tar::Archive;
use zstd::stream::read::Decoder;

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

/// Injects `RUSTC_WRAPPER=sccache` for Rust nodes when sccache is available
/// on the system PATH and the user has not already provided a `RUSTC_WRAPPER`
/// value.
///
/// User-provided values are never overwritten.
///
/// Returns `true` if `RUSTC_WRAPPER=sccache` was injected.
fn inject_rust_build_env(env_vars: &mut Vec<(String, String)>, language: PeppygenLanguage) -> bool {
    if language != PeppygenLanguage::Rust {
        return false;
    }
    let sccache_injected =
        !env_vars.iter().any(|(k, _)| k == "RUSTC_WRAPPER") && is_sccache_available();
    if sccache_injected {
        env_vars.push(("RUSTC_WRAPPER".to_string(), "sccache".to_string()));
    }
    sccache_injected
}

pub(crate) fn generate_random_id() -> String {
    let mut rng = rand::rng();
    let bytes: [u8; 3] = rng.random();
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Extracts a `.tar.zst` archive into `destination` with path safety checks.
/// Rejects entries containing `..`, root, or prefix path components.
/// Directories are applied last to avoid permission interference during extraction.
pub(crate) fn extract_tar_zst(
    archive_path: &Path,
    destination: &Path,
) -> std::result::Result<(), String> {
    let file = std::fs::File::open(archive_path)
        .map_err(|e| format!("Failed to open archive {}: {}", archive_path.display(), e))?;

    let decoder = Decoder::new(file).map_err(|e| {
        format!(
            "Failed to decode zstd archive {}: {}",
            archive_path.display(),
            e
        )
    })?;
    let mut archive = Archive::new(decoder);

    let entries = archive.entries().map_err(|e| {
        format!(
            "Failed to read archive entries from {}: {}",
            archive_path.display(),
            e
        )
    })?;

    let mut directories = Vec::new();
    for entry in entries {
        let mut entry = entry.map_err(|e| {
            format!(
                "Failed to read archive entry from {}: {}",
                archive_path.display(),
                e
            )
        })?;

        let entry_path = entry
            .path()
            .map_err(|e| {
                format!(
                    "Failed to read entry path from {}: {}",
                    archive_path.display(),
                    e
                )
            })?
            .into_owned();

        if entry_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(..)
            )
        }) {
            return Err(format!(
                "Archive {} contains unsafe path: {}",
                archive_path.display(),
                entry_path.display()
            ));
        }

        if entry.header().entry_type().is_dir() {
            directories.push(entry);
        } else {
            let unpacked = entry.unpack_in(destination).map_err(|e| {
                format!(
                    "Failed to unpack entry {} from {}: {}",
                    entry_path.display(),
                    archive_path.display(),
                    e
                )
            })?;
            if !unpacked {
                return Err(format!(
                    "Archive {} contains unsafe path: {}",
                    archive_path.display(),
                    entry_path.display()
                ));
            }
        }
    }

    // Apply directory entries at the end, matching tar::Archive::unpack behavior (avoids
    // directory permissions interfering with descendant extraction).
    directories.sort_by(|a, b| b.path_bytes().cmp(&a.path_bytes()));
    for mut dir in directories {
        let entry_path = dir
            .path()
            .map_err(|e| {
                format!(
                    "Failed to read entry path from {}: {}",
                    archive_path.display(),
                    e
                )
            })?
            .into_owned();
        let unpacked = dir.unpack_in(destination).map_err(|e| {
            format!(
                "Failed to unpack entry {} from {}: {}",
                entry_path.display(),
                archive_path.display(),
                e
            )
        })?;
        if !unpacked {
            return Err(format!(
                "Archive {} contains unsafe path: {}",
                archive_path.display(),
                entry_path.display()
            ));
        }
    }

    Ok(())
}

pub use add::listen_for_node_add;
pub use info::listen_for_node_info;
pub use init::listen_for_node_init;
pub use remove::listen_for_node_remove;
pub use start::listen_for_node_start;
pub use stop::listen_for_node_stop;
pub use sync::listen_for_node_sync;

pub(crate) async fn resolve_node_config(
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
        inject_rust_build_env(&mut env_vars, PeppygenLanguage::Python);
        assert!(
            !env_vars.iter().any(|(k, _)| k == "RUSTC_WRAPPER"),
            "RUSTC_WRAPPER should not be set for Python nodes"
        );
    }

    #[test]
    fn inject_rust_build_env_respects_user_overrides() {
        let mut env_vars = vec![("RUSTC_WRAPPER".to_string(), "custom_wrapper".to_string())];
        inject_rust_build_env(&mut env_vars, PeppygenLanguage::Rust);
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
    }

    #[test]
    fn is_sccache_available_does_not_panic() {
        let _ = is_sccache_available();
    }
}
