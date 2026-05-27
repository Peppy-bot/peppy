use super::super::action_loop::{ActionResult, ActionState, GoalHandler, run_action_loop};
use super::super::stack::STACK_LAUNCH_GIT_HASH;
use super::gate::ConcurrencyGate;
use super::sync::{
    self, AutoSyncParams, collect_consumed_interfaces, generate_peppygen_for_node,
    resolve_conforms_to, stack_resolver,
};
use super::{
    clone_with_progress, extract_tar_zst, format_bytes, generate_random_id,
    is_supported_fs_archive, is_supported_http_archive, locate_node_root_dir,
    resolve_local_archive_source, sanitize_repo_path, write_error_to_log,
};
use crate::Result;
use crate::names;
use chrono::Local;
use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH, PeppyDirs};
use config::node::validate_dependency_specs;
use config::node::{NodeConfig, NodeConfigParser};
use core_node_api::encoding::{
    NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse, NodeAddResult, NodeSource,
};
use futures::FutureExt;
use node_stack::add_steps::{copy_node_to_temp_dir, verify_git_hash};
use node_stack::{InstanceState, NodeStack, WorkingDirGuard};
use parking_lot::Mutex as StdMutex;
use peppylib::messaging::{ActionFeedbackPublisher, ServiceRequestContext};
use peppylib::types::Payload;
use peppylib::{ActionMessenger, MessengerHandle, PeppyResult};
use std::fs::File;
use std::io::{Read, Write};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use ureq::Error as HttpError;

use super::{FeedbackLine, FeedbackStream, create_action_log_file};
use peppylib::messaging::SenderTarget;

pub async fn listen_for_node_add(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
) -> Result<JoinHandle<Result<()>>> {
    let action = ActionMessenger::expose(
        messenger,
        core_node_name,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        names::NODE_ADD_ACTION,
    )
    .await?;

    let handler = NodeAddGoalHandler {
        context: NodeAddActionContext {
            node_stack,
            messenger: messenger.clone(),
            bound_core_node: core_node_name.to_string(),
            core_instance_id: instance_id.to_string(),
            peppy_dirs,
        },
        gate: ConcurrencyGate::new(),
    };

    let handle = tokio::spawn(async move { run_action_loop(action, handler).await });

    Ok(handle)
}

impl ActionResult for NodeAddResult {
    fn identifier() -> &'static str {
        "node_add_result"
    }

    fn encode_result(&self) -> crate::Result<Payload> {
        self.encode().map_err(Into::into)
    }
}

#[derive(Clone)]
struct NodeAddGoalHandler {
    context: NodeAddActionContext,
    gate: ConcurrencyGate,
}

impl GoalHandler for NodeAddGoalHandler {
    type Result = NodeAddResult;

    async fn handle_goal(
        &self,
        context: ServiceRequestContext,
        user_payload: bytes::Bytes,
        feedback_publisher: ActionFeedbackPublisher,
        state: Arc<Mutex<ActionState<NodeAddResult>>>,
    ) -> PeppyResult<Payload> {
        handle_goal_request(
            context,
            user_payload,
            feedback_publisher,
            state,
            self.context.clone(),
            self.gate.clone(),
        )
        .await
    }
}

pub(super) struct CleanupDir(Option<PathBuf>);

impl CleanupDir {
    pub(super) fn new(dir: Option<PathBuf>) -> Self {
        Self(dir)
    }

    pub(super) fn take(&mut self) -> Option<PathBuf> {
        self.0.take()
    }
}

impl Drop for CleanupDir {
    fn drop(&mut self) {
        if let Some(dir) = self.0.take() {
            std::fs::remove_dir_all(dir).ok();
        }
    }
}

pub(crate) struct ResolvedNodeAddSource {
    pub(crate) source_path: PathBuf,
    pub(crate) node_config: NodeConfig,
    pub(crate) cleanup_dir: Option<PathBuf>,
}

/// Result of downloading and extracting an HTTP bundle without parsing the node config.
pub(crate) struct ExtractedHttpSource {
    pub(crate) source_path: PathBuf,
    pub(crate) cleanup_dir: Option<PathBuf>,
}

/// Distinguishes between a definitively invalid config (no point doing a full
/// clone) and an infrastructure failure during the shallow probe (fall back to
/// full clone).
enum ShallowCheckError {
    /// The config was found but is invalid — a full clone would fail the same way.
    InvalidConfig(String),
    /// The shallow fetch itself failed (e.g. server doesn't support shallow
    /// clones, network error, ref not found). Fall back to full clone.
    ShallowFetchFailed(String),
}

/// Performs a depth-1 shallow fetch into a bare repo and reads the node config
/// blob directly from the git object database — no working-directory checkout.
fn shallow_validate_config(
    repo_url: &str,
    repo_relative_path: &Path,
    repo_ref: Option<&str>,
) -> std::result::Result<NodeConfig, ShallowCheckError> {
    let bare_dir = tempfile::tempdir().map_err(|e| {
        ShallowCheckError::ShallowFetchFailed(format!("Failed to create temp dir: {}", e))
    })?;

    let bare_repo = git2::Repository::init_bare(bare_dir.path()).map_err(|e| {
        ShallowCheckError::ShallowFetchFailed(format!("Failed to init bare repo: {}", e))
    })?;

    let mut remote = bare_repo.remote_anonymous(repo_url).map_err(|e| {
        ShallowCheckError::ShallowFetchFailed(format!("Failed to create remote: {}", e))
    })?;

    // Build refspecs to try, in order of preference.
    let refspecs: Vec<String> = match repo_ref {
        None | Some("") => vec!["HEAD".to_string()],
        Some(r) => vec![
            r.to_string(),
            format!("refs/heads/{r}"),
            format!("refs/tags/{r}"),
        ],
    };

    let mut last_err = None;
    for refspec in &refspecs {
        let mut fetch_opts = git2::FetchOptions::new();
        fetch_opts.depth(1);
        fetch_opts.download_tags(git2::AutotagOption::None);

        match remote.fetch(&[refspec.as_str()], Some(&mut fetch_opts), None) {
            Ok(()) => {
                last_err = None;
                break;
            }
            Err(e) => {
                last_err = Some(e);
                // Recreate the remote for a clean retry.
                remote = bare_repo.remote_anonymous(repo_url).map_err(|e2| {
                    ShallowCheckError::ShallowFetchFailed(format!(
                        "Failed to recreate remote: {}",
                        e2
                    ))
                })?;
            }
        }
    }
    if let Some(e) = last_err {
        return Err(ShallowCheckError::ShallowFetchFailed(format!(
            "shallow fetch failed: {}",
            e
        )));
    }

    // Resolve FETCH_HEAD to a commit.
    let fetch_head = bare_repo.find_reference("FETCH_HEAD").map_err(|e| {
        ShallowCheckError::ShallowFetchFailed(format!("Failed to find FETCH_HEAD: {}", e))
    })?;
    let commit = fetch_head.peel_to_commit().map_err(|e| {
        ShallowCheckError::ShallowFetchFailed(format!("Failed to peel to commit: {}", e))
    })?;
    let tree = commit
        .tree()
        .map_err(|e| ShallowCheckError::ShallowFetchFailed(format!("Failed to get tree: {}", e)))?;

    // Navigate the tree to find the config blob.
    let config_tree_path = if repo_relative_path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json5"))
    {
        repo_relative_path.to_path_buf()
    } else {
        repo_relative_path.join(NODE_CONFIG_FILE)
    };

    let entry = tree.get_path(&config_tree_path).map_err(|e| {
        ShallowCheckError::InvalidConfig(format!(
            "Config file '{}' not found in repository: {}",
            config_tree_path.display(),
            e
        ))
    })?;

    let blob = bare_repo.find_blob(entry.id()).map_err(|e| {
        ShallowCheckError::ShallowFetchFailed(format!("Failed to read config blob: {}", e))
    })?;

    let content = std::str::from_utf8(blob.content()).map_err(|_| {
        ShallowCheckError::InvalidConfig("peppy.json5 is not valid UTF-8".to_string())
    })?;

    NodeConfigParser::from_content(content).map_err(|e| {
        ShallowCheckError::InvalidConfig(format!(
            "Failed to parse node config at {}: {}",
            config_tree_path.display(),
            e
        ))
    })
}

#[derive(Clone)]
pub(crate) struct NodeAddActionContext {
    pub(crate) node_stack: Arc<NodeStack>,
    pub(crate) messenger: MessengerHandle,
    pub(crate) bound_core_node: String,
    pub(crate) core_instance_id: String,
    pub(crate) peppy_dirs: PeppyDirs,
}

struct ProcessNodeAddContext {
    action: NodeAddActionContext,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    log_file: Arc<StdMutex<File>>,
    log_path: PathBuf,
}

async fn resolve_git_source(
    repo_url: &gix_url::Url,
    repo_path: &str,
    repo_ref: Option<&str>,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
) -> std::result::Result<ResolvedNodeAddSource, String> {
    let repo_relative_path = sanitize_repo_path(repo_path)?;

    let repo_url_bstring = repo_url.to_bstring();
    let repo_url_str = std::str::from_utf8(repo_url_bstring.as_ref())
        .map_err(|_| "repo_url must be valid UTF-8".to_string())?
        .to_owned();

    // --- Phase 1: Shallow probe — validate peppy.json5 without a full clone ---
    let _ = feedback_tx.send(FeedbackLine {
        stream: FeedbackStream::Stdout,
        line: "Checking node config...".to_string(),
    });

    let probe_url = repo_url_str.clone();
    let probe_path = repo_relative_path.clone();
    let probe_ref = repo_ref.map(str::to_owned);

    let phase1_result = tokio::task::spawn_blocking(move || {
        shallow_validate_config(&probe_url, &probe_path, probe_ref.as_deref())
    })
    .await
    .map_err(|e| format!("Failed to join shallow probe task: {}", e))?;

    // The shallow probe is only a preflight hint: if it rejects the config,
    // fail fast; otherwise proceed to the full clone and reparse the config
    // from the actual checkout. Never use the probe's parsed value as the
    // final NodeConfig — the clone is the source of truth.
    match phase1_result {
        Ok(_) => {}
        Err(ShallowCheckError::InvalidConfig(msg)) => return Err(msg),
        Err(ShallowCheckError::ShallowFetchFailed(reason)) => {
            let _ = feedback_tx.send(FeedbackLine {
                stream: FeedbackStream::Stdout,
                line: format!(
                    "Shallow probe unavailable ({}), proceeding with full clone",
                    reason
                ),
            });
        }
    }

    // --- Phase 2: Full clone (only reached if config is valid or probe fell back) ---
    let checkout_dir = tempfile::tempdir()
        .map_err(|e| format!("Failed to create temporary directory: {}", e))?
        .keep();

    let clone_checkout_dir = checkout_dir.clone();
    let clone_repo_url = repo_url_str.clone();
    let clone_repo_ref = repo_ref.map(str::to_owned);
    let clone_feedback_tx = feedback_tx.clone();
    if let Err(err) = tokio::task::spawn_blocking(move || {
        clone_with_progress(
            &clone_repo_url,
            clone_repo_ref.as_deref(),
            &clone_checkout_dir,
            false,
            &mut |line| {
                let _ = clone_feedback_tx.send(FeedbackLine {
                    stream: FeedbackStream::Stdout,
                    line: line.to_owned(),
                });
            },
        )
        .map(|_| ())
    })
    .await
    .map_err(|e| format!("Failed to join git clone task: {}", e))?
    {
        std::fs::remove_dir_all(&checkout_dir).ok();
        return Err(err);
    }

    let candidate_path = checkout_dir.join(&repo_relative_path);

    let config_path = if candidate_path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json5"))
    {
        candidate_path
    } else {
        candidate_path.join(NODE_CONFIG_FILE)
    };

    let node_root_dir = config_path
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| "Invalid repo_path: node config has no parent directory".to_string())?;

    // Always parse the config from the cloned checkout. The shallow probe
    // earlier in this function is only a preflight hint — its result is not
    // trusted as the final config, since the probe and the clone could in
    // principle disagree (e.g. a force-push between the two fetches).
    let node_config = match NodeConfigParser::from_path(&config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            let msg = format!(
                "Failed to parse node config at {}: {}",
                config_path.display(),
                e
            );
            let _ = feedback_tx.send(FeedbackLine {
                stream: FeedbackStream::Stderr,
                line: msg.clone(),
            });
            std::fs::remove_dir_all(&checkout_dir).ok();
            return Err(msg);
        }
    };

    Ok(ResolvedNodeAddSource {
        source_path: node_root_dir,
        node_config,
        cleanup_dir: Some(checkout_dir),
    })
}

fn sanitize_http_filename(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => ch,
            _ => '_',
        })
        .collect();
    let sanitized = sanitized.trim_matches('_');
    if sanitized.is_empty() {
        "bundle.tar.zst".to_string()
    } else {
        sanitized.to_string()
    }
}

fn bundle_file_name(url: &url::Url) -> String {
    url.path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|segment| !segment.is_empty())
        .map(sanitize_http_filename)
        .unwrap_or_else(|| "bundle.tar.zst".to_string())
}

fn download_http_bundle(
    url: &url::Url,
    destination: &Path,
    expected_sha256: Option<&str>,
    feedback_tx: Option<&mpsc::UnboundedSender<FeedbackLine>>,
) -> std::result::Result<(), String> {
    use sha2::{Digest, Sha256};

    let expected_bytes = expected_sha256
        .map(|hex| decode_sha256_hex(hex).map_err(|e| format!("Invalid SHA256 for {}: {}", url, e)))
        .transpose()?;

    let response = ureq::get(url.as_str()).call().map_err(|err| {
        let reason = match err {
            HttpError::StatusCode(code) => format!("unexpected status code {code}"),
            other => other.to_string(),
        };
        format!("Failed to download bundle from {}: {}", url, reason)
    })?;

    let total_size: Option<u64> = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok());

    let mut reader = response.into_body().into_reader();
    let mut file = File::create(destination).map_err(|e| {
        format!(
            "Failed to create bundle file {}: {}",
            destination.display(),
            e
        )
    })?;

    let mut hasher = expected_bytes.as_ref().map(|_| Sha256::new());
    let mut buffer = [0u8; 8 * 1024];
    let mut bytes_downloaded: u64 = 0;
    let mut last_report = Instant::now();
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|e| format!("Failed to read response body from {}: {}", url, e))?;
        if read == 0 {
            break;
        }
        bytes_downloaded += read as u64;
        file.write_all(&buffer[..read]).map_err(|e| {
            format!(
                "Failed to write bundle file {}: {}",
                destination.display(),
                e
            )
        })?;
        if let Some(ref mut h) = hasher {
            h.update(&buffer[..read]);
        }
        if let Some(tx) = feedback_tx
            && last_report.elapsed() >= Duration::from_millis(500)
        {
            last_report = Instant::now();
            let progress_msg = if let Some(total) = total_size {
                let pct = if total > 0 {
                    (bytes_downloaded as f64 / total as f64 * 100.0) as u64
                } else {
                    0
                };
                format!(
                    "Downloading: {} / {} ({}%)",
                    format_bytes(bytes_downloaded as usize),
                    format_bytes(total as usize),
                    pct,
                )
            } else {
                format!("Downloading: {}", format_bytes(bytes_downloaded as usize))
            };
            let _ = tx.send(FeedbackLine {
                stream: FeedbackStream::Stdout,
                line: progress_msg,
            });
        }
    }
    file.flush().map_err(|e| {
        format!(
            "Failed to flush bundle file {}: {}",
            destination.display(),
            e
        )
    })?;

    if let Some((hasher, expected)) = hasher.zip(expected_bytes) {
        let computed = hasher.finalize();
        if computed.as_slice() != expected.as_slice() {
            std::fs::remove_file(destination).ok();
            return Err(format!(
                "SHA256 checksum mismatch for {}: expected {}, computed {}",
                url,
                expected_sha256.unwrap_or_default(),
                encode_hex(computed.as_slice()),
            ));
        }
    }

    Ok(())
}

fn decode_sha256_hex(input: &str) -> std::result::Result<Vec<u8>, String> {
    let bytes = input.as_bytes();
    if bytes.len() != 64 {
        return Err(format!("expected 64 hex characters, got {}", bytes.len()));
    }
    // Input is already validated as lowercase hex by config parsing (source.rs),
    // so we can convert directly without error handling per digit.
    let mut output = Vec::with_capacity(32);
    for chunk in bytes.chunks_exact(2) {
        let high = (chunk[0] as char).to_digit(16).unwrap() as u8;
        let low = (chunk[1] as char).to_digit(16).unwrap() as u8;
        output.push((high << 4) | low);
    }
    Ok(output)
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn extract_http_bundle(
    bundle_path: &Path,
    destination: &Path,
    url: &url::Url,
) -> std::result::Result<(), String> {
    extract_tar_zst(bundle_path, destination).map_err(|e| format!("{} (source: {})", e, url))
}

pub(crate) async fn download_and_extract_http_source(
    url: &url::Url,
    peppy_dirs: PeppyDirs,
    expected_sha256: Option<String>,
) -> std::result::Result<ExtractedHttpSource, String> {
    let url = url.clone();
    tokio::task::spawn_blocking(move || {
        resolve_http_source_download_and_extract(url, &peppy_dirs, expected_sha256, None)
    })
    .await
    .map_err(|e| format!("Failed to join HTTP download task: {}", e))?
}

pub(crate) async fn resolve_http_source(
    url: &url::Url,
    peppy_dirs: PeppyDirs,
    expected_sha256: Option<String>,
) -> std::result::Result<ResolvedNodeAddSource, String> {
    let url = url.clone();
    tokio::task::spawn_blocking(move || {
        resolve_http_source_blocking(url, &peppy_dirs, expected_sha256, None)
    })
    .await
    .map_err(|e| format!("Failed to join HTTP download task: {}", e))?
}

async fn resolve_http_source_with_feedback(
    url: &url::Url,
    peppy_dirs: PeppyDirs,
    expected_sha256: Option<String>,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
) -> std::result::Result<ResolvedNodeAddSource, String> {
    let url = url.clone();
    tokio::task::spawn_blocking(move || {
        resolve_http_source_blocking(url, &peppy_dirs, expected_sha256, Some(&feedback_tx))
    })
    .await
    .map_err(|e| format!("Failed to join HTTP download task: {}", e))?
}

fn resolve_http_source_download_and_extract(
    url: url::Url,
    peppy_dirs: &PeppyDirs,
    expected_sha256: Option<String>,
    feedback_tx: Option<&mpsc::UnboundedSender<FeedbackLine>>,
) -> std::result::Result<ExtractedHttpSource, String> {
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "HTTP source URL must use http or https (got scheme '{}')",
                other
            ));
        }
    }

    if !is_supported_http_archive(&url) {
        return Err(
            "Only tar.zst (.tar.zstd/.tar.zst/.tzst) archives are supported for HTTP sources"
                .to_string(),
        );
    }

    let http_base_dir = peppy_dirs.http_downloads_dir();
    std::fs::create_dir_all(&http_base_dir)
        .map_err(|e| format!("Failed to create HTTP download directory: {}", e))?;

    let timestamp = Local::now().format("%Y%m%d_%H%M%S_%3f");
    let operation_dir =
        http_base_dir.join(format!("node_add_{timestamp}_{}", generate_random_id()));
    std::fs::create_dir_all(&operation_dir)
        .map_err(|e| format!("Failed to create HTTP staging directory: {}", e))?;

    let mut operation_cleanup = CleanupDir::new(Some(operation_dir.clone()));

    let bundle_path = operation_dir.join(bundle_file_name(&url));
    let extract_dir = operation_dir.join("extracted");
    std::fs::create_dir_all(&extract_dir)
        .map_err(|e| format!("Failed to create bundle extract directory: {}", e))?;

    download_http_bundle(&url, &bundle_path, expected_sha256.as_deref(), feedback_tx)?;
    extract_http_bundle(&bundle_path, &extract_dir, &url)?;
    std::fs::remove_file(&bundle_path).ok();

    let node_root_dir = locate_node_root_dir(&extract_dir)?;

    let operation_dir = operation_cleanup.take();

    Ok(ExtractedHttpSource {
        source_path: node_root_dir,
        cleanup_dir: operation_dir,
    })
}

fn resolve_http_source_blocking(
    url: url::Url,
    peppy_dirs: &PeppyDirs,
    expected_sha256: Option<String>,
    feedback_tx: Option<&mpsc::UnboundedSender<FeedbackLine>>,
) -> std::result::Result<ResolvedNodeAddSource, String> {
    let extracted =
        resolve_http_source_download_and_extract(url, peppy_dirs, expected_sha256, feedback_tx)?;
    let config_path = extracted.source_path.join(NODE_CONFIG_FILE);
    let node_config = NodeConfigParser::from_path(&config_path).map_err(|e| {
        format!(
            "Failed to parse node config at {}: {}",
            config_path.display(),
            e
        )
    })?;

    Ok(ResolvedNodeAddSource {
        source_path: extracted.source_path,
        node_config,
        cleanup_dir: extracted.cleanup_dir,
    })
}

/// Derives a label for the log filename from the NodeSource without network I/O.
/// For Fs sources, reads the local config to get `{name}_{tag}` (fast, local I/O only).
/// For Git/Http sources, returns a UUID since the node name/tag are unknown before cloning.
pub(crate) fn log_label_from_source(source: &NodeSource) -> String {
    match source {
        NodeSource::Fs(path) => {
            if is_supported_fs_archive(path)
                && let Ok(resolved) = resolve_local_archive_source(path)
            {
                let label = format!(
                    "{}_{}",
                    resolved.node_config.manifest.name.as_str(),
                    resolved.node_config.manifest.tag
                );
                return label;
            }

            let config_path = path.join(NODE_CONFIG_FILE);
            if let Ok(config) = NodeConfigParser::from_path(&config_path) {
                return format!("{}_{}", config.manifest.name.as_str(), config.manifest.tag);
            }
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string()
        }
        NodeSource::Git { .. } | NodeSource::Http { .. } => generate_random_id(),
        NodeSource::RepoNode { name, tag, .. } => format!("{name}_{tag}"),
    }
}

async fn resolve_node_add_source(
    goal: &NodeAddGoal,
    peppy_dirs: &PeppyDirs,
    feedback_tx: &mpsc::UnboundedSender<FeedbackLine>,
) -> std::result::Result<ResolvedNodeAddSource, String> {
    match &goal.source {
        NodeSource::Fs(path) => {
            // When using the stack-launch marker, skip git hash verification and fingerprint
            // checks. This allows stack_launch to work with local filesystem sources without
            // requiring `peppy node sync` beforehand - fresh peppygen will be generated.
            let is_stack_launch = goal.git_hash == STACK_LAUNCH_GIT_HASH;

            if is_supported_fs_archive(path) {
                let resolved = resolve_local_archive_source(path)?;
                if !is_stack_launch {
                    verify_git_hash(&resolved.source_path, &goal.git_hash)?;
                }
                return Ok(ResolvedNodeAddSource {
                    source_path: resolved.source_path,
                    node_config: resolved.node_config,
                    cleanup_dir: Some(resolved.temp_dir.keep()),
                });
            }

            let config_path = path.join(NODE_CONFIG_FILE);
            let node_config = NodeConfigParser::from_path(&config_path).map_err(|e| {
                format!(
                    "Failed to parse node config at {}: {}",
                    config_path.display(),
                    e
                )
            })?;
            Ok(ResolvedNodeAddSource {
                source_path: path.clone(),
                node_config,
                cleanup_dir: None,
            })
        }
        NodeSource::Git {
            repo_url,
            repo_path,
            repo_ref,
        } => {
            let _ = feedback_tx.send(FeedbackLine {
                stream: FeedbackStream::Stdout,
                line: format!("Cloning repository {}...", repo_url.to_bstring()),
            });
            resolve_git_source(
                repo_url,
                repo_path,
                repo_ref.as_deref(),
                feedback_tx.clone(),
            )
            .await
        }
        NodeSource::Http { url, sha256 } => {
            let _ = feedback_tx.send(FeedbackLine {
                stream: FeedbackStream::Stdout,
                line: format!("Downloading bundle from {}...", url),
            });
            resolve_http_source_with_feedback(
                url,
                peppy_dirs.clone(),
                sha256.clone(),
                feedback_tx.clone(),
            )
            .await
        }
        NodeSource::RepoNode { .. } => {
            // RepoNode goals are dispatched to `add_batch::run_repo_node_add`
            // by `handle_goal_request` and never reach `run_node_add`, so this
            // arm should be unreachable by construction.
            Err("internal error: RepoNode reached the single-source add path".to_owned())
        }
    }
}

/// Encodes a rejected goal response, mapping encoding errors to `PeppyError`.
fn encode_rejected_goal(reason: impl Into<String>) -> PeppyResult<Payload> {
    super::encode_response_or_err(
        "node_add_goal",
        NodeAddGoalResponse::rejected(reason).encode(),
    )
}

/// Runs the full node-add pipeline: resolves the source, renames the log file
/// to its canonical form, calls [`process_node_add`], and catches panics.
///
/// The caller is responsible for creating the log file (so the action-server
/// path can include its path in the goal response before spawning this).
///
/// This is the shared implementation used by both the action-server path
/// ([`handle_goal_request`]) and the direct-call path from `stack_launch`.
pub(crate) async fn run_node_add(
    goal: NodeAddGoal,
    action_context: NodeAddActionContext,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    log_file: Arc<StdMutex<File>>,
    log_path: PathBuf,
    timestamp: String,
) -> NodeAddResult {
    let log_dir = action_context.peppy_dirs.logs_dir_add();

    let log_file_for_panic = log_file.clone();
    let log_path_for_panic = log_path.clone();

    match AssertUnwindSafe(async {
        // Resolve source (git clone, HTTP download, or local config check).
        let mut resolved =
            match resolve_node_add_source(&goal, &action_context.peppy_dirs, &feedback_tx).await {
                Ok(r) => r,
                Err(error_msg) => {
                    write_error_to_log(&log_file, &error_msg);
                    return NodeAddResult::failure(&log_path, error_msg);
                }
            };

        // RAII guard: ensures resolved.cleanup_dir is removed on any early return
        // before process_node_add takes ownership.
        let mut resolved_cleanup_guard = CleanupDir::new(resolved.cleanup_dir.take());

        let root_source_path = resolved.source_path.clone();
        let node_config: NodeConfig = resolved.node_config;
        let root_execution_language = node_config.execution.language;

        // Auto-generate .peppy directory if missing (e.g. fresh clone never synced).
        // Must run before git hash verification since sync also writes git.hash.
        //
        // Generation and filesystem I/O are blocking; offload to spawn_blocking
        // to avoid stalling the Tokio runtime (mirrors the daemon node-sync path).
        if goal.git_hash != STACK_LAUNCH_GIT_HASH
            && let NodeSource::Fs(original_path) = &goal.source
            && !is_supported_fs_archive(original_path)
        {
            let sync_node_dir = root_source_path.clone();
            let sync_execution_language = root_execution_language;
            let sync_manifest = node_config.manifest.clone();
            let sync_interfaces = node_config.interfaces.clone();
            let sync_git_hash = goal.git_hash.clone();
            let sync_node_stack = action_context.node_stack.clone();
            let sync_peppy_dirs = action_context.peppy_dirs.clone();
            let sync_feedback_tx = feedback_tx.clone();

            let sync_result = tokio::task::spawn_blocking(move || {
                let on_feedback = |line: &str| {
                    tracing::info!(target: "peppy::interface", "{line}");
                    let _ = sync_feedback_tx.send(FeedbackLine {
                        stream: FeedbackStream::Stdout,
                        line: line.to_string(),
                    });
                };
                sync::auto_sync_if_missing(
                    AutoSyncParams {
                        node_dir: &sync_node_dir,
                        execution_language: sync_execution_language,
                        manifest: &sync_manifest,
                        interfaces: &sync_interfaces,
                        git_hash: &sync_git_hash,
                        on_feedback: &on_feedback,
                    },
                    &sync_node_stack,
                    &sync_peppy_dirs,
                )
            })
            .await;

            match sync_result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    let msg = format!("Auto-sync failed: {}", e);
                    write_error_to_log(&log_file, &msg);
                    return NodeAddResult::failure(&log_path, msg);
                }
                Err(e) => {
                    let msg = format!("Auto-sync task failed: {}", e);
                    write_error_to_log(&log_file, &msg);
                    return NodeAddResult::failure(&log_path, msg);
                }
            }
        }

        // Verify git hash for non-archive local FS sources.
        //
        // The .peppy/git.hash file is always written by `peppy node sync` at
        // the root level (alongside the peppy.json5 that contains the manifest).
        if goal.git_hash != STACK_LAUNCH_GIT_HASH
            && let NodeSource::Fs(original_path) = &goal.source
            && !is_supported_fs_archive(original_path)
            && let Err(error_msg) = verify_git_hash(&root_source_path, &goal.git_hash)
        {
            write_error_to_log(&log_file, &error_msg);
            return NodeAddResult::failure(&log_path, error_msg);
        }

        let cleanup_dir = resolved_cleanup_guard.take();
        let source_path = resolved.source_path.clone();

        // Fingerprints are only generated locally by `peppy node sync`.
        // Git/Http sources never have them, so skip verification for remote sources.
        let verify_codegen_fingerprint =
            matches!(&goal.source, NodeSource::Fs(_)) && goal.git_hash != STACK_LAUNCH_GIT_HASH;

        // Rename log file to the canonical {name}_{tag}_{timestamp}.log format
        // now that we know the node name and tag from the resolved config.
        let node_name = node_config.manifest.name.as_str();
        let node_tag = &node_config.manifest.tag;
        let canonical_filename = format!("{}_{}_{}.log", node_name, node_tag, timestamp);
        let canonical_log_path = log_dir.join(&canonical_filename);
        let log_path = if std::fs::rename(&log_path, &canonical_log_path).is_ok() {
            canonical_log_path
        } else {
            log_path
        };

        let ctx = ProcessNodeAddContext {
            action: action_context,
            feedback_tx,
            log_file,
            log_path,
        };
        process_node_add(
            goal,
            node_config,
            source_path,
            verify_codegen_fingerprint,
            cleanup_dir,
            ctx,
        )
        .await
    })
    .catch_unwind()
    .await
    {
        Ok(result) => result,
        Err(panic_payload) => {
            let msg = format!(
                "node_add task panicked: {}",
                super::panic_message(&*panic_payload)
            );
            tracing::error!("{}", msg);
            write_error_to_log(&log_file_for_panic, &msg);
            NodeAddResult::failure(log_path_for_panic, msg)
        }
    }
}

async fn handle_goal_request(
    context: ServiceRequestContext,
    user_payload: bytes::Bytes,
    feedback_publisher: ActionFeedbackPublisher,
    state: Arc<Mutex<ActionState<NodeAddResult>>>,
    action_context: NodeAddActionContext,
    gate: ConcurrencyGate,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id();

    let goal = match NodeAddGoal::decode(&user_payload) {
        Ok(g) => g,
        Err(e) => return encode_rejected_goal(format!("invalid payload: {}", e)),
    };

    {
        let mut state_guard = state.lock().await;
        if goal.force && matches!(*state_guard, ActionState::Running { .. }) {
            debug!("Force flag set: aborting previous node_add task");
        }
        if let super::gate::Admission::AlreadyRunning { remaining_secs } =
            gate.try_admit(&mut state_guard, goal.timeout_secs, goal.force)
        {
            return encode_rejected_goal(format!(
                "action already in progress (times out in {remaining_secs}s), \
                 use `--force` to force adding the node"
            ));
        }
    }

    match &goal.source {
        NodeSource::Fs(path) => debug!(
            "Received `node_add` goal from {sender_instance_id}, source={}",
            path.display()
        ),
        NodeSource::Git {
            repo_url,
            repo_path,
            repo_ref,
        } => debug!(
            "Received `node_add` goal from {sender_instance_id}, source=git:{}::{} ({:?})",
            repo_url, repo_path, repo_ref
        ),
        NodeSource::Http { url, .. } => debug!(
            "Received `node_add` goal from {sender_instance_id}, source=http:{}",
            url
        ),
        NodeSource::RepoNode { name, tag, .. } => debug!(
            "Received `node_add` goal from {sender_instance_id}, source=repo:{}:{}",
            name, tag
        ),
    }

    // Create the log file *before* source resolution so that clone/download
    // progress and any errors are captured in the log from the very start.
    let log_label = log_label_from_source(&goal.source);
    let log_dir = action_context.peppy_dirs.logs_dir_add();
    let timestamp = Local::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    let log_filename = format!("{}_{}.log", log_label, timestamp);
    let (log_file, log_path) = match create_action_log_file(&log_dir, &log_filename) {
        Ok(result) => result,
        Err(error_msg) => {
            debug!("{}", error_msg);
            let mut state_guard = state.lock().await;
            *state_guard = ActionState::Rejected;
            return encode_rejected_goal(error_msg);
        }
    };

    debug!("Created log file for node add: {}", log_path.display());

    let state_clone = Arc::clone(&state);
    let log_path_clone = log_path.clone();
    let cancel_token = CancellationToken::new();
    let cancel_token_clone = cancel_token.clone();
    let log_path_for_cancel = log_path.clone();
    let task_handle = tokio::spawn(async move {
        let (feedback_tx, feedback_rx) = mpsc::unbounded_channel::<FeedbackLine>();
        let consumer_handle =
            super::spawn_feedback_forwarder(feedback_rx, feedback_publisher.clone(), |line| {
                NodeAddFeedback::from_stream(line.stream, &line.line).encode()
            });

        let is_repo_node = matches!(&goal.source, NodeSource::RepoNode { .. });
        let result = tokio::select! {
            biased;
            result = async {
                if is_repo_node {
                    super::add_batch::run_repo_node_add(
                        goal,
                        action_context,
                        feedback_tx,
                        log_file,
                        log_path_clone,
                    ).await
                } else {
                    run_node_add(
                        goal,
                        action_context,
                        feedback_tx,
                        log_file,
                        log_path_clone,
                        timestamp,
                    ).await
                }
            } => result,
            _ = cancel_token_clone.cancelled() => {
                NodeAddResult::failure(
                    &log_path_for_cancel,
                    "add cancelled by --force".to_string(),
                )
            }
        };

        // Wait for the feedback consumer to drain before completing.
        let _ = consumer_handle.await;
        let mut state_guard = state_clone.lock().await;
        *state_guard = ActionState::Completed { result };
    });

    gate.set_task(task_handle, cancel_token);

    super::encode_response_or_err(
        "node_add_goal",
        NodeAddGoalResponse::accepted(&log_path).encode(),
    )
}

async fn shutdown_existing_instances(
    node_name: &str,
    node_tag: &str,
    ctx: &ProcessNodeAddContext,
) -> std::result::Result<(), String> {
    let Some(entity) = ctx.action.node_stack.find(node_name, node_tag) else {
        return Ok(());
    };

    let instances = {
        let guard = entity.read();
        if guard.instances().is_empty() {
            return Ok(());
        }
        // Fail fast if any instance is mid-start: stop_instance cannot reach
        // it (no messenger subscriptions yet) and proceeding to push_config
        // could leave the node partially down. The caller can retry once the
        // start has either committed (Running) or aborted.
        if guard
            .instances()
            .iter()
            .any(|instance| instance.state() == InstanceState::Starting)
        {
            return Err(format!(
                "node '{}:{}' has an instance in Starting state; cannot overwrite a node with live instances",
                node_name, node_tag
            ));
        }
        guard
            .instances()
            .iter()
            .filter(|instance| instance.state() == InstanceState::Running)
            .map(|instance| instance.instance_id().clone())
            .collect::<Vec<_>>()
    };

    for instance_id in instances {
        let instance_id_str = instance_id.as_str().to_owned();
        debug!(
            "Shutting down existing node instance {}, node={}:{}",
            instance_id_str, node_name, node_tag
        );

        super::stop::stop_instance(
            &ctx.action.messenger,
            &ctx.action.bound_core_node,
            &ctx.action.core_instance_id,
            &ctx.action.node_stack,
            node_name,
            node_tag,
            &instance_id,
        )
        .await?;

        let _ = ctx.feedback_tx.send(FeedbackLine {
            stream: FeedbackStream::Stdout,
            line: format!("{instance_id_str} has been stopped"),
        });
    }

    Ok(())
}

async fn process_node_add(
    goal: NodeAddGoal,
    node_config: NodeConfig,
    source_path: PathBuf,
    verify_codegen_fingerprint: bool,
    cleanup_dir: Option<PathBuf>,
    ctx: ProcessNodeAddContext,
) -> NodeAddResult {
    // Reject any forbidden env vars early so the user gets a fast failure;
    // the values themselves are not used by the add path (env vars are
    // consumed by `node_build`'s `build_cmd` execution).
    if let Err(e) = super::validate_goal_env_vars(&goal.env_vars) {
        let msg = e.to_string();
        write_error_to_log(&ctx.log_file, &msg);
        return NodeAddResult::failure(&ctx.log_path, msg);
    }
    let node_name = node_config.manifest.name.as_str().to_owned();
    let node_tag = node_config.manifest.tag.clone();
    let _cleanup_guard = CleanupDir::new(cleanup_dir);

    if verify_codegen_fingerprint {
        // Verify that the node config fingerprint matches the one in the generated folder.
        // Even if the `.peppy` content will be regenerated in the copy, we want to make sure the code that
        // the user has written for their node matches the current interface version.
        let config_path = source_path.join(NODE_CONFIG_FILE);
        if let Err(e) =
            config::fingerprint::verify_codegen_fingerprint(&config_path, PEPPYGEN_OUTPUT_PATH)
        {
            let msg = format!("Codegen fingerprint verification failed: {}", e);
            write_error_to_log(&ctx.log_file, &msg);
            return NodeAddResult::failure(&ctx.log_path, msg);
        }
    }

    // Copy the node folder to a temporary working directory.
    let (working_dir, excluded_dirs) =
        match copy_node_to_temp_dir(&source_path, &ctx.action.peppy_dirs.tmp_dir()) {
            Ok(result) => result,
            Err(e) => {
                let msg = format!("Failed to copy node folder: {}", e);
                write_error_to_log(&ctx.log_file, &msg);
                return NodeAddResult::failure(&ctx.log_path, msg);
            }
        };
    // RAII guard: cleans up the temp working dir on any exit path.
    let mut working_dir_cleanup = CleanupDir::new(Some(working_dir.clone()));

    if !excluded_dirs.is_empty() {
        let _ = ctx.feedback_tx.send(FeedbackLine {
            stream: FeedbackStream::Stdout,
            line: format!(
                "Excluded directories from copy: {}",
                excluded_dirs.join(", ")
            ),
        });
    }

    // Validate that all dependency nodes exist in the stack and expose the required
    // interfaces before running build_cmd. This prevents confusing build failures when
    // peppygen is generated with incomplete interfaces due to missing dependencies.
    let dep_errors = validate_dependency_specs(
        &node_config.manifest,
        &node_config.interfaces,
        &node_name,
        &node_tag,
        |name, tag| {
            ctx.action
                .node_stack
                .find(name, tag)
                .map(|e| e.read().config().clone())
        },
    );
    if let Some(err) = dep_errors.into_iter().next() {
        let msg = format!("Failed to add node config: {}", err);
        write_error_to_log(&ctx.log_file, &msg);
        return NodeAddResult::failure(&ctx.log_path, msg);
    }

    // Generate the peppygen library in the working directory.
    // Container builds need Copy mode because Apptainer's `%files` copies symlinks
    // as-is — absolute symlinks to the host cache would be broken inside the container.
    let language = node_config.execution.language;
    let deploy_mode = if node_config.execution.container.is_some() {
        generator::CrateDeployMode::Copy
    } else {
        generator::CrateDeployMode::Symlink
    };
    let interface_feedback = |line: &str| {
        tracing::info!(target: "peppy::interface", "{line}");
    };
    let mut consumed_interfaces = match collect_consumed_interfaces(
        &node_config.manifest,
        &node_config.interfaces,
        stack_resolver(&ctx.action.node_stack),
        &ctx.action.peppy_dirs,
        &interface_feedback,
    ) {
        Ok(v) => v,
        Err(reason) => {
            let msg = format!("Failed to resolve consumed interfaces: {}", reason);
            write_error_to_log(&ctx.log_file, &msg);
            return NodeAddResult::failure(&ctx.log_path, msg);
        }
    };
    let conformed = match resolve_conforms_to(
        &node_config.interfaces,
        &ctx.action.peppy_dirs,
        &interface_feedback,
    ) {
        Ok(v) => v,
        Err(reason) => {
            let msg = format!("Failed to resolve `conforms_to` interfaces: {}", reason);
            write_error_to_log(&ctx.log_file, &msg);
            return NodeAddResult::failure(&ctx.log_path, msg);
        }
    };
    consumed_interfaces.extend(conformed);
    if let Err(e) = generate_peppygen_for_node(
        language,
        &working_dir,
        consumed_interfaces,
        &goal.git_hash,
        &ctx.action.peppy_dirs,
        deploy_mode,
        None,
    ) {
        let msg = format!("Failed to generate peppygen library: {}", e);
        write_error_to_log(&ctx.log_file, &msg);
        return NodeAddResult::failure(&ctx.log_path, msg);
    }

    // Stop any pre-existing instances of this node before pushing the new
    // config. `push_config` rejects replacements that still have live
    // instances (it would otherwise orphan them), so we shut them down
    // first to satisfy that precondition.
    if let Err(e) = shutdown_existing_instances(&node_name, &node_tag, &ctx).await {
        let msg = format!("Failed to shutdown existing node instances: {}", e);
        write_error_to_log(&ctx.log_file, &msg);
        return NodeAddResult::failure(&ctx.log_path, msg);
    }

    // Push the node config into the stack as an `Added` entity. Use the
    // working_dir copy of peppy.json5 rather than source_path because
    // source_path may point at a transient Git/Http clone that is cleaned
    // up after this function returns. The working_dir persists as long as
    // the entity exists via WorkingDirGuard.
    let config_path_for_stack = working_dir.join(NODE_CONFIG_FILE);
    if let Err(e) =
        ctx.action
            .node_stack
            .push_config(node_config.clone(), false, &config_path_for_stack)
    {
        let msg = format!("Failed to add node config: {}", e);
        write_error_to_log(&ctx.log_file, &msg);
        return NodeAddResult::failure(&ctx.log_path, msg);
    }

    let entity_handle = match ctx.action.node_stack.find(&node_name, &node_tag) {
        Some(handle) => handle,
        None => {
            let msg = format!(
                "internal error: just-pushed entity {}:{} disappeared from the stack",
                node_name, node_tag
            );
            write_error_to_log(&ctx.log_file, &msg);
            return NodeAddResult::failure(&ctx.log_path, msg);
        }
    };

    // Hand over the temporary working dir to the entity so a follow-up
    // `node_build` can reuse it without re-cloning the source. The
    // `WorkingDirGuard` cleans the directory up on entity removal.
    let working_dir_guard = Arc::new(WorkingDirGuard::new(
        working_dir_cleanup
            .take()
            .expect("working_dir_cleanup was just constructed Some"),
    ));
    {
        let mut guard = entity_handle.write();
        ctx.action
            .node_stack
            .set_add_log_path(&node_name, &node_tag, ctx.log_path.clone());
        guard.set_pending_working_dir(Arc::clone(&working_dir_guard));
    }

    debug!("Added node {}:{} (pending build)", node_name, node_tag);

    NodeAddResult::success(&ctx.log_path, node_name, node_tag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use httptest::{Expectation, Server, matchers::request, responders::status_code};
    use sha2::{Digest, Sha256};

    fn create_test_tar_zst(content: &[u8]) -> Vec<u8> {
        let mut tar_data = Vec::new();
        {
            let mut tar_builder = tar::Builder::new(&mut tar_data);
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar_builder
                .append_data(&mut header, "test.txt", content)
                .unwrap();
            tar_builder.finish().unwrap();
        }
        let mut zst_data = Vec::new();
        let mut encoder = zstd::Encoder::new(&mut zst_data, 0).unwrap();
        std::io::Write::write_all(&mut encoder, &tar_data).unwrap();
        encoder.finish().unwrap();
        zst_data
    }

    fn sha256_hex(data: &[u8]) -> String {
        let hash = Sha256::digest(data);
        encode_hex(hash.as_slice())
    }

    #[test]
    fn test_download_http_bundle_checksum_mismatch() {
        let bundle = create_test_tar_zst(b"hello world");
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/bundle.tar.zst"))
                .respond_with(status_code(200).body(bundle)),
        );

        let url = url::Url::parse(&server.url("/bundle.tar.zst").to_string()).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("bundle.tar.zst");

        let wrong_hash = "a".repeat(64);
        let result = download_http_bundle(&url, &dest, Some(&wrong_hash), None);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("checksum mismatch"),
            "error should mention checksum mismatch, got: {}",
            err
        );
        assert!(
            !dest.exists(),
            "bundle file should be cleaned up on mismatch"
        );
    }

    #[test]
    fn test_download_http_bundle_checksum_ok() {
        let bundle = create_test_tar_zst(b"hello world");
        let correct_hash = sha256_hex(&bundle);
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/bundle.tar.zst"))
                .respond_with(status_code(200).body(bundle)),
        );

        let url = url::Url::parse(&server.url("/bundle.tar.zst").to_string()).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("bundle.tar.zst");

        let result = download_http_bundle(&url, &dest, Some(&correct_hash), None);
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        assert!(
            dest.exists(),
            "bundle file should exist after successful download"
        );
    }

    #[test]
    fn test_download_http_bundle_no_checksum_skips_verification() {
        let bundle = create_test_tar_zst(b"hello world");
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/bundle.tar.zst"))
                .respond_with(status_code(200).body(bundle)),
        );

        let url = url::Url::parse(&server.url("/bundle.tar.zst").to_string()).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("bundle.tar.zst");

        let result = download_http_bundle(&url, &dest, None, None);
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        assert!(dest.exists(), "bundle file should exist");
    }
}
