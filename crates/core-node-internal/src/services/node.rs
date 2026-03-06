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
pub use add::listen_for_node_add;
use chrono::Local;
use config::node::{NodeConfig, PeppygenLanguage};
use git2::{Repository, build::CheckoutBuilder};
pub use info::listen_for_node_info;
pub use init::listen_for_node_init;
use rand::RngExt;
pub use remove::listen_for_node_remove;
pub use start::{NodeStartServiceConfig, listen_for_node_start};
use std::fs::File;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
pub use stop::listen_for_node_stop;
pub use sync::listen_for_node_sync;
use tar::Archive;
use zstd::stream::read::Decoder;

/// Maximum number of stderr lines to retain for error diagnostics.
/// Used by both the `add` (container build) and `start` (node run) services.
const STDERR_TAIL_LINES: usize = 20;

/// Extract a human-readable message from a panic payload.
/// Used by spawned task handlers to convert panics into failure results.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

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

const PEPPY_APPTAINER_BIN_ENV_VAR: &str = "PEPPY_APPTAINER_BIN";
const PEPPY_NODE_NAME_ENV_VAR: &str = "PEPPY_NODE_NAME";
const PEPPY_NODE_TAG_ENV_VAR: &str = "PEPPY_NODE_TAG";

/// Validates environment variables, rejecting any that are in the forbidden list.
/// Returns an error if any forbidden env var is found.
fn validate_goal_env_vars(env_vars: &[(String, String)]) -> Result<Vec<(String, String)>> {
    let mut result = Vec::with_capacity(env_vars.len());
    for (key, value) in env_vars {
        let normalized = key.trim().to_ascii_uppercase();
        if FORBIDDEN_ENV_KEYS.contains(&normalized.as_str()) {
            return Err(Error::ForbiddenEnvVar(normalized));
        }
        result.push((key.trim().to_string(), value.clone()));
    }
    Ok(result)
}

/// Write an error message to the node's log file with a timestamp.
///
/// Best-effort: silently ignores lock/write failures since the error is also
/// returned in the result encoding.
fn write_error_to_log(log_file: &Arc<StdMutex<File>>, error_msg: &str) {
    if let Ok(mut file) = log_file.lock() {
        let timestamp = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
        let _ = writeln!(file, "[{}] [error] {}", timestamp, error_msg);
        let _ = file.flush();
    }
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

fn inject_node_runtime_env(env_vars: &mut Vec<(String, String)>, node_name: &str, node_tag: &str) {
    push_env_if_missing(
        env_vars,
        PEPPY_APPTAINER_BIN_ENV_VAR,
        resolve_apptainer_bin(),
    );
    push_env_if_missing(env_vars, PEPPY_NODE_NAME_ENV_VAR, node_name.to_string());
    push_env_if_missing(env_vars, PEPPY_NODE_TAG_ENV_VAR, node_tag.to_string());
}

pub(crate) fn generate_random_id() -> String {
    let mut rng = rand::rng();
    let bytes: [u8; 6] = rng.random();
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

pub(crate) fn sanitize_repo_path(repo_path: &str) -> std::result::Result<PathBuf, String> {
    let trimmed = repo_path.trim_start_matches(['/', '\\']);
    let path = PathBuf::from(trimmed);
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("repo_path must not contain '..'".to_string());
    }
    Ok(path)
}

pub(crate) fn checkout_repo_ref(
    repo: &Repository,
    repo_ref: &str,
) -> std::result::Result<(), git2::Error> {
    let repo_ref = repo_ref.trim();
    if repo_ref.is_empty() {
        return Ok(());
    }
    let object = repo
        .revparse_single(repo_ref)
        .or_else(|_| repo.revparse_single(&format!("refs/tags/{repo_ref}")))
        .or_else(|_| repo.revparse_single(&format!("refs/heads/{repo_ref}")))
        .or_else(|_| repo.revparse_single(&format!("refs/remotes/origin/{repo_ref}")))?;
    let commit = object.peel_to_commit()?;
    repo.set_head_detached(commit.id())?;
    let mut checkout = CheckoutBuilder::new();
    checkout.force();
    repo.checkout_head(Some(&mut checkout))?;
    Ok(())
}

pub(crate) fn is_supported_http_archive(url: &url::Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    path.ends_with(".tar.zst") || path.ends_with(".tar.zstd") || path.ends_with(".tzst")
}

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

    #[test]
    fn inject_node_runtime_env_sets_expected_keys() {
        let mut env_vars = Vec::new();
        inject_node_runtime_env(&mut env_vars, "uvc_camera", "0.1.0");

        let apptainer_bin = env_vars
            .iter()
            .find(|(k, _)| k == PEPPY_APPTAINER_BIN_ENV_VAR)
            .map(|(_, v)| v)
            .expect("PEPPY_APPTAINER_BIN should be injected");
        assert!(
            !apptainer_bin.trim().is_empty(),
            "PEPPY_APPTAINER_BIN should not be empty"
        );

        assert_eq!(
            env_vars
                .iter()
                .find(|(k, _)| k == PEPPY_NODE_NAME_ENV_VAR)
                .map(|(_, v)| v.as_str()),
            Some("uvc_camera")
        );
        assert_eq!(
            env_vars
                .iter()
                .find(|(k, _)| k == PEPPY_NODE_TAG_ENV_VAR)
                .map(|(_, v)| v.as_str()),
            Some("0.1.0")
        );
    }

    #[test]
    fn inject_node_runtime_env_keeps_existing_values() {
        let mut env_vars = vec![
            (
                PEPPY_APPTAINER_BIN_ENV_VAR.to_string(),
                "/custom/apptainer".to_string(),
            ),
            (
                PEPPY_NODE_NAME_ENV_VAR.to_string(),
                "custom_node".to_string(),
            ),
            (PEPPY_NODE_TAG_ENV_VAR.to_string(), "9.9.9".to_string()),
        ];

        inject_node_runtime_env(&mut env_vars, "uvc_camera", "0.1.0");

        assert_eq!(
            env_vars
                .iter()
                .filter(|(k, _)| k == PEPPY_APPTAINER_BIN_ENV_VAR)
                .count(),
            1
        );
        assert_eq!(
            env_vars
                .iter()
                .find(|(k, _)| k == PEPPY_APPTAINER_BIN_ENV_VAR)
                .map(|(_, v)| v.as_str()),
            Some("/custom/apptainer")
        );
        assert_eq!(
            env_vars
                .iter()
                .find(|(k, _)| k == PEPPY_NODE_NAME_ENV_VAR)
                .map(|(_, v)| v.as_str()),
            Some("custom_node")
        );
        assert_eq!(
            env_vars
                .iter()
                .find(|(k, _)| k == PEPPY_NODE_TAG_ENV_VAR)
                .map(|(_, v)| v.as_str()),
            Some("9.9.9")
        );
    }

    #[test]
    fn sanitize_repo_path_accepts_relative_path() {
        let result = sanitize_repo_path("some/path");
        assert_eq!(result.unwrap(), PathBuf::from("some/path"));
    }

    #[test]
    fn sanitize_repo_path_strips_leading_slashes() {
        let result = sanitize_repo_path("///some/path");
        assert_eq!(result.unwrap(), PathBuf::from("some/path"));
    }

    #[test]
    fn sanitize_repo_path_strips_leading_backslashes() {
        let result = sanitize_repo_path("\\\\some\\path");
        assert_eq!(result.unwrap(), PathBuf::from("some\\path"));
    }

    #[test]
    fn sanitize_repo_path_rejects_parent_dir() {
        let result = sanitize_repo_path("some/../etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains(".."));
    }

    #[test]
    fn is_supported_http_archive_accepts_tar_zst() {
        let url = url::Url::parse("https://example.com/bundle.tar.zst").unwrap();
        assert!(is_supported_http_archive(&url));
    }

    #[test]
    fn is_supported_http_archive_accepts_tar_zstd() {
        let url = url::Url::parse("https://example.com/bundle.tar.zstd").unwrap();
        assert!(is_supported_http_archive(&url));
    }

    #[test]
    fn is_supported_http_archive_accepts_tzst() {
        let url = url::Url::parse("https://example.com/bundle.tzst").unwrap();
        assert!(is_supported_http_archive(&url));
    }

    #[test]
    fn is_supported_http_archive_rejects_tar_gz() {
        let url = url::Url::parse("https://example.com/bundle.tar.gz").unwrap();
        assert!(!is_supported_http_archive(&url));
    }

    #[test]
    fn is_supported_http_archive_rejects_plain_url() {
        let url = url::Url::parse("https://example.com/page").unwrap();
        assert!(!is_supported_http_archive(&url));
    }

    #[test]
    fn is_supported_http_archive_is_case_insensitive() {
        let url = url::Url::parse("https://example.com/BUNDLE.TAR.ZST").unwrap();
        assert!(is_supported_http_archive(&url));
    }

    #[test]
    fn validate_goal_env_vars_preserves_key_casing() {
        let env_vars = vec![
            ("my_Custom_Var".to_string(), "value1".to_string()),
            ("AnotherVar".to_string(), "value2".to_string()),
        ];
        let result = validate_goal_env_vars(&env_vars).unwrap();
        assert_eq!(result[0].0, "my_Custom_Var");
        assert_eq!(result[1].0, "AnotherVar");
    }

    #[test]
    fn validate_goal_env_vars_trims_key_whitespace() {
        let env_vars = vec![("  MY_VAR  ".to_string(), "value".to_string())];
        let result = validate_goal_env_vars(&env_vars).unwrap();
        assert_eq!(result[0].0, "MY_VAR");
    }

    #[test]
    fn validate_goal_env_vars_rejects_forbidden_keys_case_insensitively() {
        let env_vars = vec![("ld_preload".to_string(), "evil.so".to_string())];
        let err = validate_goal_env_vars(&env_vars).unwrap_err();
        assert!(err.to_string().contains("LD_PRELOAD"));
    }

    #[test]
    fn panic_message_extracts_str_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("something broke");
        assert_eq!(panic_message(&*payload), "something broke");
    }

    #[test]
    fn panic_message_extracts_string_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(String::from("detailed error"));
        assert_eq!(panic_message(&*payload), "detailed error");
    }

    #[test]
    fn panic_message_handles_unknown_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(42i32);
        assert_eq!(panic_message(&*payload), "unknown panic payload");
    }
}
