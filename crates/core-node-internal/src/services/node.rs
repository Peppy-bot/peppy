mod add;
mod builder;
mod gate;
mod info;
mod init;
mod remove;
mod run;
mod stop;
mod sync;
mod templates;
pub(crate) mod variant;

use crate::encoding::NodeSource;
use crate::{Error, Result};
pub use add::listen_for_node_add;
pub(crate) use add::{NodeAddActionContext, log_label_from_source, run_node_add};
pub use builder::listen_for_node_build;
pub(crate) use builder::{NodeBuildActionContext, run_node_build_for_entity};
use chrono::Local;
use config::consts::{NODE_CONFIG_FILE, PeppyDirs};
use config::node::{NodeConfig, NodeConfigParser, ParsedNodeConfig, PeppygenLanguage};
use git2::{Repository, build::CheckoutBuilder, build::RepoBuilder};
pub use info::listen_for_node_info;
pub use init::listen_for_node_init;
use parking_lot::Mutex as StdMutex;
use rand::RngExt;
pub use remove::listen_for_node_remove;
pub(crate) use run::{NodeRunActionContext, run_node_run};
pub use run::{NodeRunServiceConfig, listen_for_node_run};
use std::fs::File;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;
pub use stop::listen_for_node_stop;
pub use sync::listen_for_node_sync;

// Feedback streaming primitives have moved to `node-stack-internal::build_io`
// so that `NodeEntity::build` can stream apptainer output without depending on
// core-node-internal. The re-exports below keep the existing call sites in
// `start.rs`, `info.rs`, etc. compiling unchanged.
pub(crate) use node_stack::{FeedbackLine, FeedbackStream};

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
/// Used by both the core node (to reject requests) and CLI (to filter before sending).
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

/// Maps an encoding `Result` to a `PeppyResult`, wrapping the error as an
/// `InternalEncodingError` so it can be returned directly from a goal handler.
/// Used in place of open-coding the same `map_err` at every rejection and
/// accepted-response encoding site in the add/build/start handlers.
pub(crate) fn encode_response_or_err(
    identifier: &'static str,
    result: crate::Result<peppylib::types::Payload>,
) -> peppylib::PeppyResult<peppylib::types::Payload> {
    result.map_err(|e| peppylib::PeppyError::InternalEncodingError {
        identifier: identifier.to_string(),
        reason: format!("Failed to encode response: {}", e),
    })
}

/// Spawns a task that consumes `FeedbackLine` values from `feedback_rx`,
/// converts each one via `encode` and publishes the resulting payload. Shared
/// by the add/build/start goal handlers, which all run the same
/// consumer-side forwarder over differently-typed feedback encoders.
pub(crate) fn spawn_feedback_forwarder<F>(
    mut feedback_rx: tokio::sync::mpsc::UnboundedReceiver<FeedbackLine>,
    publisher: peppylib::messaging::TopicPublisher,
    encode: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn(FeedbackLine) -> crate::Result<peppylib::types::Payload> + Send + 'static,
{
    tokio::spawn(async move {
        while let Some(line) = feedback_rx.recv().await {
            if let Ok(payload) = encode(line) {
                let _ = publisher.publish(payload).await;
            }
        }
    })
}

/// Write an error message to the node's log file with a timestamp.
///
/// Best-effort: silently ignores lock/write failures since the error is also
/// returned in the result encoding.
pub(crate) fn write_error_to_log(log_file: &Arc<StdMutex<File>>, error_msg: &str) {
    let mut file = log_file.lock();
    let timestamp = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
    let _ = writeln!(file, "[{}] [error] {}", timestamp, error_msg);
    let _ = file.flush();
}

/// Appends a timestamped entry to the stack operations log.
///
/// Best-effort: silently ignores I/O failures since the operation it
/// describes has already completed.
pub(crate) fn append_stack_log(peppy_dirs: &PeppyDirs, message: &str) {
    let path = peppy_dirs.stack_log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let timestamp = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
    let _ = writeln!(file, "[{}] {}", timestamp, message);
}

/// Creates a log file inside `log_dir` with the given filename.
///
/// Creates the directory tree if it doesn't exist. Returns the log file
/// handle (wrapped for concurrent access) and its path.
pub(crate) fn create_action_log_file(
    log_dir: &Path,
    log_filename: &str,
) -> std::result::Result<(Arc<StdMutex<File>>, PathBuf), String> {
    std::fs::create_dir_all(log_dir)
        .map_err(|e| format!("Failed to create logs directory: {}", e))?;

    let log_path = log_dir.join(log_filename);
    let file = File::create(&log_path).map_err(|e| format!("Failed to create log file: {}", e))?;

    Ok((Arc::new(StdMutex::new(file)), log_path))
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

pub(crate) struct ResolvedLocalArchiveSource {
    pub(crate) node_config: ParsedNodeConfig,
    pub(crate) source_path: PathBuf,
    pub(crate) temp_dir: tempfile::TempDir,
}

pub(crate) use node_stack::extract_tar_zst;

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

/// Clones a git repository, aborting the network transfer if `deadline` is
/// exceeded.  When `deadline` is `None` the clone runs without any time limit.
pub(crate) fn clone_repo_with_deadline(
    repo_url: &str,
    dest: &Path,
    deadline: Option<Instant>,
) -> std::result::Result<Repository, String> {
    let deadline_triggered = Arc::new(AtomicBool::new(false));

    let mut callbacks = git2::RemoteCallbacks::new();
    if let Some(deadline) = deadline {
        let flag = Arc::clone(&deadline_triggered);
        callbacks.transfer_progress(move |_progress| {
            if Instant::now() >= deadline {
                flag.store(true, Ordering::SeqCst);
                return false;
            }
            true
        });
    }

    let mut fetch_opts = git2::FetchOptions::new();
    fetch_opts.remote_callbacks(callbacks);

    RepoBuilder::new()
        .fetch_options(fetch_opts)
        .clone(repo_url, dest)
        .map_err(|e| {
            if deadline_triggered.load(Ordering::SeqCst) {
                format!("Git clone timed out for {}", repo_url)
            } else {
                format!("Failed to clone repository: {}", e)
            }
        })
}

fn is_supported_archive_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.ends_with(".tar.zst") || path.ends_with(".tar.zstd") || path.ends_with(".tzst")
}

pub(crate) fn is_supported_fs_archive(path: &Path) -> bool {
    is_supported_archive_path(path.to_string_lossy().as_ref())
}

pub(crate) fn locate_node_root_dir(extracted_dir: &Path) -> std::result::Result<PathBuf, String> {
    let direct = extracted_dir.join(NODE_CONFIG_FILE);
    if direct.is_file() {
        return Ok(extracted_dir.to_path_buf());
    }

    let mut candidate_dirs = Vec::new();
    for entry in std::fs::read_dir(extracted_dir).map_err(|e| {
        format!(
            "Failed to list extracted bundle directory {}: {}",
            extracted_dir.display(),
            e
        )
    })? {
        let entry = entry.map_err(|e| {
            format!(
                "Failed to read extracted bundle directory entry in {}: {}",
                extracted_dir.display(),
                e
            )
        })?;
        let file_type = entry.file_type().map_err(|e| {
            format!(
                "Failed to read file type for extracted bundle entry {}: {}",
                entry.path().display(),
                e
            )
        })?;
        if file_type.is_dir() {
            candidate_dirs.push(entry.path());
        }
    }

    if candidate_dirs.len() == 1 {
        let candidate = candidate_dirs.pop().expect("candidate dir should exist");
        if candidate.join(NODE_CONFIG_FILE).is_file() {
            return Ok(candidate);
        }
    }

    Err(format!(
        "Bundle does not contain {} at the root (or single top-level folder)",
        NODE_CONFIG_FILE
    ))
}

pub(crate) fn resolve_local_archive_source(
    archive_path: &Path,
) -> std::result::Result<ResolvedLocalArchiveSource, String> {
    let temp_dir =
        tempfile::tempdir().map_err(|e| format!("Failed to create temporary directory: {}", e))?;

    extract_tar_zst(archive_path, temp_dir.path())?;
    let source_path = locate_node_root_dir(temp_dir.path())?;
    let config_path = source_path.join(NODE_CONFIG_FILE);
    let node_config = NodeConfigParser::from_path(&config_path).map_err(|e| {
        format!(
            "Failed to parse node config at {}: {}",
            config_path.display(),
            e
        )
    })?;

    Ok(ResolvedLocalArchiveSource {
        node_config,
        source_path,
        temp_dir,
    })
}

pub(crate) fn is_supported_http_archive(url: &url::Url) -> bool {
    is_supported_archive_path(url.path())
}

pub(crate) async fn resolve_node_config(
    source: NodeSource,
    peppy_dirs: &config::consts::PeppyDirs,
) -> std::result::Result<NodeConfig, String> {
    info::resolve_node_config(source, peppy_dirs).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_NODE_CONFIG: &str = r#"{
  schema_version: 1,
  manifest: {
    name: "standalone",
    tag: "0.1.0",
  },
  interfaces: {},
  execution: {
    language: "rust",
    parameters: {
      device: {
        physical: "string",
        sim: "string",
        priority: "string",
      },
      video: {
        frame_rate: "u16",
        resolution: {
          width: "u16",
          height: "u16",
        },
        encoding: "string",
      },
    },
    build_cmd: [
      "cargo",
      "build",
      "--release",
    ],
    run_cmd: [
      "./target/release/standalone",
    ],
  },
}"#;

    fn write_tar_zst_archive(archive_path: &Path, entries: &[(&str, Option<&str>)]) {
        let file = std::fs::File::create(archive_path).unwrap();
        let encoder = zstd::stream::write::Encoder::new(file, 1).unwrap();
        let mut builder = tar::Builder::new(encoder);

        for (path, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_path(path).unwrap();
            header.set_mode(if contents.is_some() { 0o644 } else { 0o755 });
            if let Some(contents) = contents {
                header.set_size(contents.len() as u64);
                header.set_cksum();
                builder.append(&header, contents.as_bytes()).unwrap();
            } else {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                header.set_cksum();
                builder.append(&header, std::io::empty()).unwrap();
            }
        }

        builder.finish().unwrap();
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();
    }

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
    fn is_supported_fs_archive_accepts_supported_extensions() {
        assert!(is_supported_fs_archive(Path::new("bundle.tar.zst")));
        assert!(is_supported_fs_archive(Path::new("bundle.tar.zstd")));
        assert!(is_supported_fs_archive(Path::new("bundle.tzst")));
        assert!(is_supported_fs_archive(Path::new("BUNDLE.TAR.ZST")));
        assert!(!is_supported_fs_archive(Path::new("bundle.tar.gz")));
    }

    /// Verifies that `resolve_local_archive_source` works when the config file
    /// sits directly at the archive root (no wrapping top-level folder).
    #[test]
    fn resolve_local_archive_source_accepts_root_layout() {
        let temp_dir = tempfile::tempdir().unwrap();
        let archive_path = temp_dir.path().join("bundle.tar.zst");
        write_tar_zst_archive(&archive_path, &[(NODE_CONFIG_FILE, Some(TEST_NODE_CONFIG))]);

        let resolved = resolve_local_archive_source(&archive_path).unwrap();

        assert_eq!(resolved.node_config.manifest_name(), "standalone");
        assert_eq!(resolved.source_path, resolved.temp_dir.path());
        assert!(resolved.source_path.join(NODE_CONFIG_FILE).is_file());
    }

    /// Verifies that `resolve_local_archive_source` unwraps a single top-level
    /// folder and uses it as the source path instead of the extraction root.
    #[test]
    fn resolve_local_archive_source_uses_single_top_level_folder() {
        let temp_dir = tempfile::tempdir().unwrap();
        let archive_path = temp_dir.path().join("bundle.tar.zst");
        write_tar_zst_archive(
            &archive_path,
            &[("node", None), ("node/peppy.json5", Some(TEST_NODE_CONFIG))],
        );

        let resolved = resolve_local_archive_source(&archive_path).unwrap();

        assert_eq!(resolved.node_config.manifest_name(), "standalone");
        assert_eq!(
            resolved
                .source_path
                .file_name()
                .and_then(|name| name.to_str()),
            Some("node")
        );
        assert_eq!(
            resolved.source_path.parent(),
            Some(resolved.temp_dir.path())
        );
        assert!(resolved.source_path.join(NODE_CONFIG_FILE).is_file());
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
