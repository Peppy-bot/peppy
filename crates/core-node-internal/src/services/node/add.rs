mod staged;

use self::staged::{StagedJob, run_staged_job};
use super::super::action_loop::{
    GoalEnding, GoalHandler, WorkStop, accept_goal, cancel_reason, invalid_payload, refuse_goal,
    run_action_loop, under_goal_cancel,
};
use super::super::stack::STACK_LAUNCH_GIT_HASH;
use super::gate::{COOPERATIVE_TEARDOWN_BUDGET, ConcurrencyGate};
use super::git_utils::install_credentials;
use super::sync::{
    self, AutoSyncParams, DepClosure, MaterializedDeps, collect_all_deployment_interfaces,
    materialize_repo_deps, stack_resolver, stack_then_repo_resolver,
};
use super::{
    clone_with_progress, encode_hex, extract_tar_zst, generate_random_id, is_supported_fs_archive,
    is_supported_http_archive, locate_node_root_dir, parse_node_config_file,
    resolve_local_archive_source, sanitize_repo_path,
};
use crate::Result;
use crate::source_url::SourceUrl;
use chrono::Local;
use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use config::failure_kind::PeppyFailureKind;
use config::node::{MissingDependencyPolicy, validate_dependency_specs};
use config::node::{NodeConfig, NodeConfigParser};
use core_node_api::ActionId;
use core_node_api::encoding::{
    NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse, NodeAddResult, NodeSource,
};
use core_node_api::names;
use daemon_config::consts::PeppyDirs;
use futures::FutureExt;
use node_stack::add_steps::{copy_node_to_temp_dir, verify_git_hash};
use node_stack::build_io::format_bytes;
use node_stack::{ActionLog, Announcer, InstanceState, NodeStack, NodeStackError, WorkingDirGuard};
use peppylib::MessengerHandle;
use peppylib::messaging::{ConcurrentAction, Failure, PendingGoal};
use std::fs::File;
use std::io::{Read, Write};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::debug;
use ureq::Error as HttpError;

use super::{FeedbackLine, interface_step_sink};
use peppylib::messaging::SenderTarget;

#[allow(clippy::too_many_arguments)] // Mirrors the other listeners' identity args + shared handles.
pub async fn listen_for_node_add(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
    relationships: super::RelationshipCoordinators,
    slice_ownership: Arc<crate::services::federation::SliceOwnership>,
) -> Result<JoinHandle<Result<()>>> {
    let action = ConcurrentAction::expose(
        messenger,
        core_node_name,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        ActionId::NodeAdd.name(),
        true,
    )
    .await?;

    let handler = NodeAddGoalHandler {
        context: NodeAddActionContext {
            node_stack,
            messenger: messenger.clone(),
            bound_core_node: core_node_name.to_string(),
            core_instance_id: instance_id.to_string(),
            peppy_dirs,
            relationships,
        },
        gate: ConcurrencyGate::new(),
        slice_ownership,
    };

    let handle = tokio::spawn(async move { run_action_loop(action, handler).await });

    Ok(handle)
}

#[derive(Clone)]
struct NodeAddGoalHandler {
    context: NodeAddActionContext,
    gate: ConcurrencyGate,
    /// Consulted before admission: while a federated launch holds this machine,
    /// only that launch's own dispatch may touch the stack. Lives on the
    /// handler rather than the action context because the in-process launch
    /// path calls the runner directly and is the coordinator's own work.
    slice_ownership: Arc<crate::services::federation::SliceOwnership>,
}

impl GoalHandler for NodeAddGoalHandler {
    async fn handle_goal(&self, pending: PendingGoal) {
        handle_goal_request(
            pending,
            self.context.clone(),
            self.gate.clone(),
            &self.slice_ownership,
        )
        .await
    }
}

struct CleanupDir(Option<PathBuf>);

impl CleanupDir {
    fn new(dir: Option<PathBuf>) -> Self {
        Self(dir)
    }

    fn take(&mut self) -> Option<PathBuf> {
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

struct ResolvedNodeAddSource {
    source_path: PathBuf,
    node_config: NodeConfig,
    cleanup_dir: Option<PathBuf>,
}

/// Result of downloading and extracting an HTTP bundle without parsing the node config.
struct ExtractedHttpSource {
    source_path: PathBuf,
    cleanup_dir: Option<PathBuf>,
}

/// Distinguishes between a definitively invalid config (no point doing a full
/// clone) and an infrastructure failure during the shallow probe (fall back to
/// full clone).
enum ShallowCheckError {
    /// The config was found but is invalid; a full clone would fail the same way.
    InvalidConfig(String),
    /// The shallow fetch itself failed (e.g. server doesn't support shallow
    /// clones, network error, ref not found). Fall back to full clone.
    ShallowFetchFailed(String),
}

/// Performs a depth-1 shallow fetch into a bare repo and reads the node config
/// blob directly from the git object database, with no working-directory checkout.
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
        let mut callbacks = git2::RemoteCallbacks::new();
        install_credentials(&mut callbacks);
        let mut fetch_opts = git2::FetchOptions::new();
        fetch_opts.remote_callbacks(callbacks);
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
            SourceUrl::new(repo_url).redact_in(&e.to_string())
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
    let config_tree_path = node_config_path(repo_relative_path);

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
    /// All relationship authorities needed to tear down overwritten instances.
    pub(crate) relationships: super::RelationshipCoordinators,
}

struct ProcessNodeAddContext {
    action: NodeAddActionContext,
    announcer: Announcer,
}

async fn resolve_git_source(
    repo_url: &gix_url::Url,
    repo_path: &str,
    repo_ref: Option<&str>,
    announcer: Announcer,
) -> std::result::Result<ResolvedNodeAddSource, String> {
    let repo_relative_path = sanitize_repo_path(repo_path)?;

    let repo_url_bstring = repo_url.to_bstring();
    let repo_url_str = std::str::from_utf8(repo_url_bstring.as_ref())
        .map_err(|_| "repo_url must be valid UTF-8".to_string())?
        .to_owned();

    shallow_probe_config(&repo_url_str, &repo_relative_path, repo_ref, &announcer).await?;
    let checkout_dir = clone_repo_to_temp(&repo_url_str, repo_ref, &announcer).await?;
    parse_config_from_checkout(checkout_dir, &repo_relative_path)
}

/// Phase 1 of [`resolve_git_source`]: shallow probe to validate peppy.json5
/// without a full clone. An invalid config fails fast; a probe that cannot
/// run (shallow fetch unsupported, network error, ref not found) reports the
/// fallback via feedback and lets the full clone proceed.
async fn shallow_probe_config(
    repo_url: &str,
    repo_relative_path: &Path,
    repo_ref: Option<&str>,
    announcer: &Announcer,
) -> std::result::Result<(), String> {
    announcer.line("Checking node config...");

    let probe_url = repo_url.to_owned();
    let probe_path = repo_relative_path.to_owned();
    let probe_ref = repo_ref.map(str::to_owned);

    let probe_result = tokio::task::spawn_blocking(move || {
        shallow_validate_config(&probe_url, &probe_path, probe_ref.as_deref())
    })
    .await
    .map_err(|e| format!("Failed to join shallow probe task: {}", e))?;

    // The shallow probe is only a preflight hint: if it rejects the config,
    // fail fast; otherwise proceed to the full clone and reparse the config
    // from the actual checkout. Never use the probe's parsed value as the
    // final NodeConfig; the clone is the source of truth.
    match probe_result {
        Ok(_) => Ok(()),
        Err(ShallowCheckError::InvalidConfig(msg)) => Err(msg),
        Err(ShallowCheckError::ShallowFetchFailed(reason)) => {
            announcer.line(format!(
                "Shallow probe unavailable ({}), proceeding with full clone",
                reason
            ));
            Ok(())
        }
    }
}

/// Phase 2 of [`resolve_git_source`]: full clone into a fresh temp directory.
/// Owns the temp dir, removing it if the clone fails; on success the caller
/// takes ownership of the returned path.
async fn clone_repo_to_temp(
    repo_url: &str,
    repo_ref: Option<&str>,
    announcer: &Announcer,
) -> std::result::Result<PathBuf, String> {
    let checkout_dir = tempfile::tempdir()
        .map_err(|e| format!("Failed to create temporary directory: {}", e))?
        .keep();

    let clone_checkout_dir = checkout_dir.clone();
    let clone_repo_url = repo_url.to_owned();
    let clone_repo_ref = repo_ref.map(str::to_owned);
    let clone_announcer = announcer.clone();
    if let Err(err) = tokio::task::spawn_blocking(move || {
        clone_with_progress(
            &clone_repo_url,
            clone_repo_ref.as_deref(),
            &clone_checkout_dir,
            &mut |line| {
                clone_announcer.progress(line);
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

    Ok(checkout_dir)
}

/// The node config a repository path names: a path with a `.json5`
/// extension (of any case) is the config file itself, any other path is the
/// directory holding [`NODE_CONFIG_FILE`]. The shallow probe and the full
/// clone both resolve the repo path here, so they read the same file.
fn node_config_path(path: &Path) -> PathBuf {
    let names_config_file = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json5"));
    if names_config_file {
        return path.to_path_buf();
    }
    path.join(NODE_CONFIG_FILE)
}

/// Phase 3 of [`resolve_git_source`]: locate the node config inside the
/// cloned checkout with [`node_config_path`] and parse it. On parse failure
/// the checkout is removed.
fn parse_config_from_checkout(
    checkout_dir: PathBuf,
    repo_relative_path: &Path,
) -> std::result::Result<ResolvedNodeAddSource, String> {
    let config_path = node_config_path(&checkout_dir.join(repo_relative_path));

    let node_root_dir = config_path
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| "Invalid repo_path: node config has no parent directory".to_string())?;

    // Always parse the config from the cloned checkout. The shallow probe
    // in phase 1 is only a preflight hint; its result is not trusted as
    // the final config, since the probe and the clone could in principle
    // disagree (e.g. a force-push between the two fetches).
    let node_config = match parse_node_config_file(&config_path) {
        Ok(cfg) => cfg,
        Err(msg) => {
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

/// Total attempts (initial try plus retries) for a bundle download that
/// keeps failing transiently.
const DOWNLOAD_ATTEMPTS: usize = 3;
/// Backoff before retry N+1; short and growing, since most transient
/// failures either clear immediately or not at all.
const DOWNLOAD_RETRY_DELAYS: [Duration; DOWNLOAD_ATTEMPTS - 1] =
    [Duration::from_millis(250), Duration::from_secs(1)];

/// A failed download attempt, split by whether retrying could help.
enum DownloadAttemptError {
    /// Transport-level failure or server-side 5xx; worth retrying.
    Transient(String),
    /// Client error (4xx), checksum mismatch, or local I/O failure;
    /// retrying cannot change the outcome.
    Fatal(String),
}

/// Whether a failed HTTP request is worth retrying. Transport-level
/// failures (I/O, timeout, connection, DNS) and server-side 5xx responses
/// are transient; anything else (4xx status, bad URI, TLS setup, redirect
/// loops) will not improve on retry.
fn is_transient_http_error(err: &HttpError) -> bool {
    match err {
        HttpError::StatusCode(code) => *code >= 500,
        HttpError::Io(_)
        | HttpError::Timeout(_)
        | HttpError::ConnectionFailed
        | HttpError::HostNotFound => true,
        _ => false,
    }
}

/// Downloads the bundle at `url` to `destination`, retrying transient
/// failures up to [`DOWNLOAD_ATTEMPTS`] times with a short growing backoff.
/// Non-transient failures (see [`is_transient_http_error`]) and checksum
/// mismatches surface immediately.
fn download_http_bundle(
    url: &url::Url,
    destination: &Path,
    expected_sha256: Option<&str>,
    announcer: Option<&Announcer>,
) -> std::result::Result<(), String> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let err = match download_http_bundle_once(url, destination, expected_sha256, announcer) {
            Ok(()) => return Ok(()),
            Err(DownloadAttemptError::Fatal(msg)) => return Err(msg),
            Err(DownloadAttemptError::Transient(msg)) if attempt >= DOWNLOAD_ATTEMPTS => {
                return Err(msg);
            }
            Err(DownloadAttemptError::Transient(msg)) => msg,
        };
        let delay = DOWNLOAD_RETRY_DELAYS[attempt - 1];
        if let Some(announcer) = announcer {
            announcer.line(format!(
                    "Download attempt {attempt} of {DOWNLOAD_ATTEMPTS} failed ({err}); retrying in {delay:?}"
                ));
        }
        std::thread::sleep(delay);
    }
}

/// One download attempt. Only the HTTP request and response-body reads can
/// yield [`DownloadAttemptError::Transient`]; local file I/O and checksum
/// verification are always fatal.
fn download_http_bundle_once(
    url: &url::Url,
    destination: &Path,
    expected_sha256: Option<&str>,
    announcer: Option<&Announcer>,
) -> std::result::Result<(), DownloadAttemptError> {
    use sha2::{Digest, Sha256};

    let shown_url = SourceUrl::new(url.as_str());
    let expected_bytes = expected_sha256
        .map(|hex| {
            decode_sha256_hex(hex).map_err(|e| format!("Invalid SHA256 for {shown_url}: {e}"))
        })
        .transpose()
        .map_err(DownloadAttemptError::Fatal)?;

    let response = ureq::get(url.as_str()).call().map_err(|err| {
        let transient = is_transient_http_error(&err);
        let reason = match err {
            HttpError::StatusCode(code) => format!("unexpected status code {code}"),
            other => other.to_string(),
        };
        let reason = shown_url.redact_in(&reason);
        let msg = format!("Failed to download bundle from {shown_url}: {reason}");
        if transient {
            DownloadAttemptError::Transient(msg)
        } else {
            DownloadAttemptError::Fatal(msg)
        }
    })?;

    let total_size: Option<u64> = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok());

    let mut reader = response.into_body().into_reader();
    let mut file = File::create(destination).map_err(|e| {
        DownloadAttemptError::Fatal(format!(
            "Failed to create bundle file {}: {}",
            destination.display(),
            e
        ))
    })?;

    let mut hasher = expected_bytes.as_ref().map(|_| Sha256::new());
    let mut buffer = [0u8; 8 * 1024];
    let mut bytes_downloaded: u64 = 0;
    let mut last_report = Instant::now();
    loop {
        let read = reader.read(&mut buffer).map_err(|e| {
            // A connection dropped mid-transfer is as transient as one that
            // never opened; the retry restarts from File::create's truncate.
            DownloadAttemptError::Transient(format!(
                "Failed to read response body from {}: {}",
                shown_url, e
            ))
        })?;
        if read == 0 {
            break;
        }
        bytes_downloaded += read as u64;
        file.write_all(&buffer[..read]).map_err(|e| {
            DownloadAttemptError::Fatal(format!(
                "Failed to write bundle file {}: {}",
                destination.display(),
                e
            ))
        })?;
        if let Some(ref mut h) = hasher {
            h.update(&buffer[..read]);
        }
        if let Some(announcer) = announcer
            && last_report.elapsed() >= Duration::from_millis(500)
        {
            last_report = Instant::now();
            announcer.progress(download_progress_line(bytes_downloaded, total_size));
        }
    }
    file.flush().map_err(|e| {
        DownloadAttemptError::Fatal(format!(
            "Failed to flush bundle file {}: {}",
            destination.display(),
            e
        ))
    })?;

    if let Some((hasher, expected)) = hasher.zip(expected_bytes) {
        let computed = hasher.finalize();
        if computed.as_slice() != expected.as_slice() {
            std::fs::remove_file(destination).ok();
            return Err(DownloadAttemptError::Fatal(format!(
                "SHA256 checksum mismatch for {}: expected {}, computed {}",
                shown_url,
                expected_sha256.unwrap_or_default(),
                encode_hex(computed.as_slice()),
            )));
        }
    }

    Ok(())
}

/// The progress line of a bundle download: the bytes received so far, and
/// the total with a percentage when the server sent a `content-length`. A
/// zero total reads as 0% rather than dividing by zero.
fn download_progress_line(downloaded: u64, total: Option<u64>) -> String {
    let Some(total) = total else {
        return format!("Downloading: {}", format_bytes(downloaded));
    };
    let percent = match total {
        0 => 0,
        _ => (downloaded as f64 / total as f64 * 100.0) as u64,
    };
    format!(
        "Downloading: {} / {} ({}%)",
        format_bytes(downloaded),
        format_bytes(total),
        percent,
    )
}

fn decode_sha256_hex(input: &str) -> std::result::Result<Vec<u8>, String> {
    let bytes = input.as_bytes();
    if bytes.len() != 64 {
        return Err(format!("expected 64 hex characters, got {}", bytes.len()));
    }
    // The length check above leaves no remainder to drop.
    let (digit_pairs, _) = bytes.as_chunks::<2>();
    let mut output = Vec::with_capacity(32);
    for &[high_digit, low_digit] in digit_pairs {
        // The digest comes from the goal as the client sent it, so a digit
        // that is not hexadecimal is refused here rather than trusted.
        let (Some(high), Some(low)) = (hex_digit_value(high_digit), hex_digit_value(low_digit))
        else {
            return Err(format!("`{input}` is not a hexadecimal string"));
        };
        output.push((high << 4) | low);
    }
    Ok(output)
}

/// The value of one hexadecimal digit of either case, or `None` for any
/// other byte.
fn hex_digit_value(digit: u8) -> Option<u8> {
    (digit as char).to_digit(16).map(|value| value as u8)
}

fn extract_http_bundle(
    bundle_path: &Path,
    destination: &Path,
    url: &url::Url,
) -> std::result::Result<(), String> {
    extract_tar_zst(bundle_path, destination)
        .map_err(|e| format!("{e} (source: {})", SourceUrl::new(url.as_str())))
}

async fn resolve_http_source_with_feedback(
    url: &url::Url,
    peppy_dirs: PeppyDirs,
    expected_sha256: Option<String>,
    announcer: Announcer,
) -> std::result::Result<ResolvedNodeAddSource, String> {
    let url = url.clone();
    tokio::task::spawn_blocking(move || {
        resolve_http_source_blocking(url, &peppy_dirs, expected_sha256, Some(&announcer))
    })
    .await
    .map_err(|e| format!("Failed to join HTTP download task: {}", e))?
}

fn resolve_http_source_download_and_extract(
    url: url::Url,
    peppy_dirs: &PeppyDirs,
    expected_sha256: Option<String>,
    announcer: Option<&Announcer>,
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

    download_http_bundle(&url, &bundle_path, expected_sha256.as_deref(), announcer)?;
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
    announcer: Option<&Announcer>,
) -> std::result::Result<ResolvedNodeAddSource, String> {
    let extracted =
        resolve_http_source_download_and_extract(url, peppy_dirs, expected_sha256, announcer)?;
    let node_config = parse_node_config_file(&extracted.source_path.join(NODE_CONFIG_FILE))?;

    Ok(ResolvedNodeAddSource {
        source_path: extracted.source_path,
        node_config,
        cleanup_dir: extracted.cleanup_dir,
    })
}

/// Creates the add log of `goal`, before its source is resolved, so the steps
/// of a clone or a download land in it. The file is named after the node when
/// the source names it without network I/O, and after a random id for a git
/// or an HTTP source.
pub(crate) fn create_add_log(
    peppy_dirs: &PeppyDirs,
    node_stack: &NodeStack,
    goal: &NodeAddGoal,
) -> std::result::Result<ActionLog, String> {
    let node = node_named_by(&goal.source);
    let label = match (&node, &goal.source) {
        (Some((name, tag)), _) => format!("{name}_{tag}"),
        (None, NodeSource::Fs(path)) => path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string(),
        (None, _) => generate_random_id(),
    };
    ActionLog::for_add(
        peppy_dirs,
        node_stack.log_exporter().clone(),
        &label,
        node.as_ref()
            .map(|(name, tag)| (name.as_str(), tag.as_str())),
        goal.launch_id.as_deref(),
    )
}

/// The `(name, tag)` of the node `source` names, when it names one without
/// network I/O. A source that does not decode names none: the add reports
/// what is wrong with it.
fn node_named_by(source: &NodeSource) -> Option<(String, String)> {
    match source {
        NodeSource::Fs(path) => {
            let config = if is_supported_fs_archive(path) {
                resolve_local_archive_source(path).ok()?.node_config
            } else {
                NodeConfigParser::from_path(path.join(NODE_CONFIG_FILE)).ok()?
            };
            Some((
                config.manifest.name.as_str().to_owned(),
                config.manifest.tag.to_string(),
            ))
        }
        NodeSource::Git { .. } | NodeSource::Http { .. } => None,
        NodeSource::Pinned { pin_json5 } => {
            serde_json5::from_str::<daemon_config::repository::PinnedItem>(pin_json5)
                .ok()
                .map(|pin| (pin.name.to_string(), pin.tag.to_string()))
        }
        NodeSource::ResolveRef { name, tag } => Some((name.clone(), tag.clone())),
        NodeSource::Exposures { pins_json5 } => super::pins::decode_pins(pins_json5)
            .ok()
            .filter(|pins| !pins.is_empty())
            .map(|pins| {
                let references: Vec<daemon_config::source::ExposureRef> =
                    pins.iter().map(Into::into).collect();
                (
                    daemon_config::mcp_deployment::built_in_identity(&references)
                        .as_str()
                        .to_owned(),
                    daemon_config::mcp_deployment::BUILT_IN_TAG.to_owned(),
                )
            }),
    }
}

async fn resolve_node_add_source(
    goal: &NodeAddGoal,
    peppy_dirs: &PeppyDirs,
    announcer: &Announcer,
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

            let node_config = parse_node_config_file(&path.join(NODE_CONFIG_FILE))?;
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
            announcer.line(format!(
                "Cloning repository {}...",
                SourceUrl::new(&repo_url.to_bstring().to_string())
            ));
            resolve_git_source(repo_url, repo_path, repo_ref.as_deref(), announcer.clone()).await
        }
        NodeSource::Http { url, sha256 } => {
            announcer.line(format!(
                "Downloading bundle from {}...",
                SourceUrl::new(url.as_str())
            ));
            resolve_http_source_with_feedback(
                url,
                peppy_dirs.clone(),
                sha256.clone(),
                announcer.clone(),
            )
            .await
        }
        NodeSource::Pinned { .. }
        | NodeSource::ResolveRef { .. }
        | NodeSource::Exposures { .. } => {
            // Pinned, resolve-ref and exposures goals are routed by
            // [`dispatch_node_add`] to their own pipelines and never reach
            // `run_node_add`, so this arm should be unreachable by
            // construction.
            Err("internal error: a pinned source reached the single-source add path".to_owned())
        }
    }
}

/// Writes the message of `failure` to the add log and returns the failure,
/// so the reason is in the file the client is pointed at.
pub(crate) fn record_add_failure(announcer: &Announcer, failure: Failure) -> Failure {
    announcer.log().error(failure.message());
    failure
}

/// The one place a node-add goal picks its pipeline: an exposures source
/// registers the built-in MCP server, a pinned source (or a `name:tag`
/// reference this daemon resolves into one) is a closure of nodes and takes
/// the batch path, every other source names one thing to add and takes the
/// single-source path.
///
/// Both the action-server path ([`handle_goal_request`]) and the direct
/// call from `stack_launch` go through here, so no caller can send a
/// source down the wrong pipeline by forgetting to check.
pub(crate) async fn dispatch_node_add(
    goal: NodeAddGoal,
    action_context: NodeAddActionContext,
    announcer: Announcer,
) -> std::result::Result<NodeAddResult, Failure> {
    match goal.source {
        NodeSource::Exposures { .. } => {
            crate::services::mcp::built_in::run_built_in_add(goal, action_context, announcer).await
        }
        NodeSource::Pinned { .. } | NodeSource::ResolveRef { .. } => {
            super::add_batch::run_pinned_add(goal, action_context, announcer).await
        }
        NodeSource::Fs(_) | NodeSource::Git { .. } | NodeSource::Http { .. } => {
            run_node_add(goal, action_context, announcer).await
        }
    }
}

/// Runs the full single-source node-add pipeline: resolves the source,
/// names the node of the log, calls [`process_node_add`], and catches
/// panics.
///
/// The caller is responsible for creating the log file (so the action-server
/// path can include its path in the goal response before spawning this).
///
/// Reached through [`dispatch_node_add`], and directly from `add_batch` for
/// each node of a batch once the repository source has been expanded.
pub(crate) async fn run_node_add(
    goal: NodeAddGoal,
    action_context: NodeAddActionContext,
    announcer: Announcer,
) -> std::result::Result<NodeAddResult, Failure> {
    // The async block takes `announcer`, so the panic arm keeps its own
    // handle on the same log.
    let log = announcer.log().clone();

    match AssertUnwindSafe(async {
        // Resolve source (git clone, HTTP download, or local config check).
        let mut resolved =
            match resolve_node_add_source(&goal, &action_context.peppy_dirs, &announcer).await {
                Ok(r) => r,
                Err(error_msg) => {
                    return Err(record_add_failure(
                        &announcer,
                        Failure::new(PeppyFailureKind::AttemptFailed, error_msg),
                    ));
                }
            };

        // RAII guard: ensures resolved.cleanup_dir is removed on any early return
        // before process_node_add takes ownership.
        let mut resolved_cleanup_guard = CleanupDir::new(resolved.cleanup_dir.take());

        let root_source_path = resolved.source_path.clone();
        let node_config: NodeConfig = resolved.node_config;
        let root_execution_language = node_config.execution.language;

        // A local directory that `peppy node sync` prepares: it is auto-synced
        // when `.peppy` is missing and checked against its recorded git hash.
        // A stack launch and an archive skip both.
        let is_synced_local_directory = goal.git_hash != STACK_LAUNCH_GIT_HASH
            && matches!(&goal.source, NodeSource::Fs(path) if !is_supported_fs_archive(path));

        // Auto-generate .peppy directory if missing (e.g. fresh clone never synced).
        // Must run before git hash verification since sync also writes git.hash.
        //
        // Generation and filesystem I/O are blocking; offload to spawn_blocking
        // to avoid stalling the Tokio runtime (mirrors the daemon node-sync path).
        if is_synced_local_directory {
            let sync_node_dir = root_source_path.clone();
            let sync_execution_language = root_execution_language;
            let sync_manifest = node_config.manifest.clone();
            let sync_interfaces = node_config.interfaces.clone();
            let sync_git_hash = goal.git_hash.clone();
            let sync_node_stack = action_context.node_stack.clone();
            let sync_peppy_dirs = action_context.peppy_dirs.clone();
            let sync_announcer = announcer.clone();

            let sync_result = tokio::task::spawn_blocking(move || {
                let on_feedback = interface_step_sink(sync_announcer);
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
                    return Err(record_add_failure(
                        &announcer,
                        Failure::new(
                            PeppyFailureKind::AttemptFailed,
                            format!("Auto-sync failed: {}", e),
                        ),
                    ));
                }
                Err(e) => {
                    return Err(record_add_failure(
                        &announcer,
                        Failure::new(
                            PeppyFailureKind::AttemptFailed,
                            format!("Auto-sync task failed: {}", e),
                        ),
                    ));
                }
            }
        }

        // Verify git hash for non-archive local FS sources.
        //
        // The .peppy/git.hash file is always written by `peppy node sync` at
        // the root level (alongside the peppy.json5 that contains the manifest).
        if is_synced_local_directory
            && let Err(error_msg) = verify_git_hash(&root_source_path, &goal.git_hash)
        {
            // The node's generated code is from another commit: a sync
            // brings it up to date.
            return Err(record_add_failure(
                &announcer,
                Failure::new(PeppyFailureKind::PreconditionFailed, error_msg),
            ));
        }

        let cleanup_dir = resolved_cleanup_guard.take();
        let source_path = resolved.source_path.clone();

        // Fingerprints are only generated locally by `peppy node sync`.
        // Git/Http sources never have them, so skip verification for remote sources.
        let verify_codegen_fingerprint =
            matches!(&goal.source, NodeSource::Fs(_)) && goal.git_hash != STACK_LAUNCH_GIT_HASH;

        // The resolved config names the node: every later line of the log
        // belongs to it. The file keeps the path the goal response gave.
        announcer.log().name_node(
            node_config.manifest.name.as_str(),
            &node_config.manifest.tag,
        );

        let ctx = ProcessNodeAddContext {
            action: action_context,
            announcer,
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
            log.error(&msg);
            Err(Failure::new(PeppyFailureKind::AttemptFailed, msg))
        }
    }
}

/// Admits a `node_add` goal through the concurrency gate. Returns the
/// admitted generation, or the refusal message when another action is
/// already running (and `--force` was not set).
///
/// A forced admission cancels the add it supersedes, which then ends
/// cancelled, "superseded by a forced node add". The admission waits for
/// that end within [`COOPERATIVE_TEARDOWN_BUDGET`], and aborts a task that
/// outlasts the budget.
async fn admit_node_add_goal(
    gate: &ConcurrencyGate,
    goal: &NodeAddGoal,
) -> std::result::Result<u64, String> {
    if goal.force {
        debug!("Force flag set: cancelling any previous node_add task");
    }
    match gate.try_admit(goal.timeout_secs, goal.force) {
        super::gate::Admission::Admitted {
            generation,
            superseded,
        } => {
            if let Some(old_task) = superseded {
                let abort = old_task.abort_handle();
                if tokio::time::timeout(COOPERATIVE_TEARDOWN_BUDGET, old_task)
                    .await
                    .is_err()
                {
                    abort.abort();
                }
            }
            Ok(generation)
        }
        super::gate::Admission::AlreadyRunning { remaining_secs } => Err(format!(
            "action already in progress (times out in {remaining_secs}s), \
             use `--force` to force adding the node"
        )),
    }
}

async fn handle_goal_request(
    pending: PendingGoal,
    action_context: NodeAddActionContext,
    gate: ConcurrencyGate,
    slice_ownership: &crate::services::federation::SliceOwnership,
) {
    let sender_instance_id = pending.instance_id().to_string();

    let goal = match NodeAddGoal::decode(pending.request_bytes()) {
        Ok(g) => g,
        Err(e) => {
            refuse_goal(pending, invalid_payload(e)).await;
            return;
        }
    };

    // Before the gate, because the gate is per-action and this exclusion is
    // per-machine: a coordinator halfway through replacing this stack must not
    // race a locally-typed `peppy node add`.
    let admission = match slice_ownership.admit_node_goal(&goal) {
        Ok(admission) => admission,
        Err(reason) => {
            refuse_goal(
                pending,
                Failure::new(PeppyFailureKind::PreconditionFailed, reason),
            )
            .await;
            return;
        }
    };

    let generation = match admit_node_add_goal(&gate, &goal).await {
        Ok(generation) => generation,
        Err(rejection) => {
            refuse_goal(
                pending,
                Failure::new(PeppyFailureKind::PreconditionFailed, rejection),
            )
            .await;
            return;
        }
    };

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
            SourceUrl::new(&repo_url.to_bstring().to_string()),
            repo_path,
            repo_ref
        ),
        NodeSource::Http { url, .. } => debug!(
            "Received `node_add` goal from {sender_instance_id}, source=http:{}",
            SourceUrl::new(url.as_str())
        ),
        NodeSource::Pinned { .. } => debug!(
            "Received `node_add` goal from {sender_instance_id}, source=pinned ({} closure pin(s))",
            goal.pins_json5.len()
        ),
        NodeSource::Exposures { .. } => debug!(
            "Received `node_add` goal from {sender_instance_id}, source=exposures (built-in MCP server)"
        ),
        NodeSource::ResolveRef { name, tag } => debug!(
            "Received `node_add` goal from {sender_instance_id}, source=resolve:{}:{}",
            name, tag
        ),
    }

    let log = match create_add_log(
        &action_context.peppy_dirs,
        &action_context.node_stack,
        &goal,
    ) {
        Ok(log) => log,
        Err(error_msg) => {
            debug!("{}", error_msg);
            gate.clear_running();
            refuse_goal(
                pending,
                Failure::new(PeppyFailureKind::AttemptFailed, error_msg),
            )
            .await;
            return;
        }
    };

    let log_path = log.path();
    debug!("Created log file for node add: {}", log_path.display());

    // `accept` registers the per-goal context before replying accepted, so a
    // fast cancel/result for this goal can't miss the routing slot.
    let Some(goal_ctx) = accept_goal(
        pending,
        super::encode_response_or_err(
            "node_add_goal",
            NodeAddGoalResponse::new(&log_path).encode(),
        ),
    )
    .await
    else {
        gate.clear_running();
        return;
    };

    let feedback_publisher = goal_ctx
        .feedback_publisher()
        .expect("node_add declares a feedback topic");
    let cancel_token = slice_ownership.stack.cancellation();
    let cancel_token_clone = cancel_token.clone();
    // A forced node add and the goal's caller cancel this goal's own token; a
    // stack reset cancels the stack's, which every goal token descends from.
    let reset_token = slice_ownership.stack.cancellation();
    let log_for_cancel = log.clone();
    let gate_for_task = gate.clone();
    let task_handle = tokio::spawn(async move {
        // Frees the gate slot on every exit: explicitly before the end on the
        // normal path (via `release_then_end` below), or on unwind for a
        // panic / `--force` abort. A no-op if a later `--force` goal took over.
        let slot = gate_for_task.into_slot_guard(generation);
        let (feedback_tx, feedback_rx) = mpsc::unbounded_channel::<FeedbackLine>();
        let consumer_handle =
            super::spawn_feedback_forwarder(feedback_rx, feedback_publisher, |line| {
                NodeAddFeedback::from_stream(line.stream, &line.line).encode()
            });

        // A cancel drops the add at its next step: the goal's caller or its
        // gone sender, a stack reset, or a forced node add that supersedes
        // it. The goal then ends cancelled, with the reason of the cancel.
        let work = async {
            tokio::select! {
                biased;
                _ = cancel_token_clone.cancelled() => {
                    let reason = cancel_reason(&goal_ctx, &reset_token)
                        .unwrap_or_else(|| "superseded by a forced node add".to_owned());
                    log_for_cancel.error(&format!("node add {reason}"));
                    Err(reason)
                }
                result = dispatch_node_add(
                    goal,
                    action_context,
                    Announcer::new(log, feedback_tx),
                ) => Ok(result),
            }
        };
        let ending = match under_goal_cancel(&goal_ctx, &cancel_token_clone, work).await {
            Ok(result) => {
                // Drain the feedback consumer before ending so the
                // end-of-stream sentinel never races ahead of the last
                // feedback line.
                let _ = consumer_handle.await;
                GoalEnding::of(
                    result.map(|added| added.encode()).map_err(WorkStop::Failed),
                    None,
                )
            }
            Err(reason) => {
                // A blocking step of the dropped add (a clone or a download)
                // keeps a sender of its feedback until the step returns, so
                // the consumer stops here and the add ends now.
                consumer_handle.abort();
                let _ = consumer_handle.await;
                GoalEnding::Cancelled(reason)
            }
        };
        drop(admission);
        slot.release_then_end(&goal_ctx, ending).await;
    });

    gate.set_task(task_handle, cancel_token);
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
        // Fail fast if any instance is mid-start: stop_instances cannot reach
        // it (no messenger subscriptions yet) and proceeding to push_config
        // could leave the node partially down. The caller can retry once the
        // start has either committed (Running) or aborted.
        let starting: Vec<String> = guard
            .instances()
            .iter()
            .filter(|instance| instance.state() == InstanceState::Starting)
            .map(|instance| {
                super::gate::starting_instance(
                    instance.instance_id(),
                    instance.starts_a_restart(),
                    &ctx.action.bound_core_node,
                )
            })
            .collect();
        if !starting.is_empty() {
            return Err(format!(
                "node '{node_name}:{node_tag}' cannot be overwritten while an instance of it \
                 starts: {}; run this again once none is starting",
                starting.join("; ")
            ));
        }
        // Everything that is not `Starting` (rejected above): `Running`
        // instances are cooperatively stopped, and `restarting` and terminal
        // (`Finished`/`Failed`) instances are cleared out the same way (they
        // get no cooperative send and `stop_instance` just removes them), so
        // `push_config` does not later reject the overwrite for a node whose
        // only instance has finished or waits for a restart.
        guard
            .instances()
            .iter()
            .filter(|instance| instance.state() != InstanceState::Starting)
            .map(|instance| instance.instance_id().clone())
            .collect::<Vec<_>>()
    };

    debug!(
        "Shutting down {} existing node instance(s), node={}:{}",
        instances.len(),
        node_name,
        node_tag
    );

    // Batched like the SIGINT teardown: every cooperative shutdown is sent
    // concurrently and the instances share one grace budget, so overwriting an
    // entity with several stuck instances costs one grace window, not one per
    // instance. Per-instance graceful/force-killed logging happens daemon-side
    // inside the stop; the feedback lines below only confirm completion.
    let mut kept = super::stop::stop_instances(
        &ctx.action.messenger,
        &ctx.action.bound_core_node,
        &ctx.action.core_instance_id,
        &ctx.action.node_stack,
        node_name,
        node_tag,
        &instances,
    )
    .await;

    for instance_id in &instances {
        ctx.action
            .relationships
            .tear_down_instance(
                instance_id.as_str(),
                kept.remove(instance_id).unwrap_or_default(),
            )
            .await;
        ctx.announcer
            .line(format!("{} has been stopped", instance_id.as_str()));
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
) -> std::result::Result<NodeAddResult, Failure> {
    process_node_add_inner(
        goal,
        node_config,
        source_path,
        verify_codegen_fingerprint,
        cleanup_dir,
        &ctx,
    )
    .await
    .map_err(|failure| {
        record_add_failure(&ctx.announcer, Failure::new(failure.kind, failure.message))
    })
}

/// Fallible body of [`process_node_add`]. Every failure funnels through the
/// wrapper's single error sink (log write + the failure of the add), so
/// failure sites here return the kind and the message via `?`: the
/// `From<String>` of [`AddStepFailure`] makes a plain message
/// `attempt_failed`.
async fn process_node_add_inner(
    goal: NodeAddGoal,
    node_config: NodeConfig,
    source_path: PathBuf,
    verify_codegen_fingerprint: bool,
    cleanup_dir: Option<PathBuf>,
    ctx: &ProcessNodeAddContext,
) -> std::result::Result<NodeAddResult, AddStepFailure> {
    // Reject any forbidden env vars early so the user gets a fast failure;
    // the values themselves are not used by the add path (env vars are
    // consumed by `node_build`'s `build_cmd` execution).
    super::validate_goal_env_vars(&goal.env_vars)
        .map_err(|e| AddStepFailure::new(PeppyFailureKind::Rejected, e.to_string()))?;
    let node_name = node_config.manifest.name.as_str().to_owned();
    let node_tag = node_config.manifest.tag.clone();
    let _cleanup_guard = CleanupDir::new(cleanup_dir);

    if verify_codegen_fingerprint {
        // Verify that the node config fingerprint matches the one in the generated folder.
        // Even if the `.peppy` content will be regenerated in the copy, we want to make sure the code that
        // the user has written for their node matches the current interface version.
        let config_path = source_path.join(NODE_CONFIG_FILE);
        config::fingerprint::verify_codegen_fingerprint(&config_path, PEPPYGEN_OUTPUT_PATH)
            .map_err(|e| {
                AddStepFailure::new(
                    PeppyFailureKind::PreconditionFailed,
                    format!("Codegen fingerprint verification failed: {}", e),
                )
            })?;
    }

    // Copy the node folder to a temporary working directory. That copy is
    // the node's staging directory, owned from here on by one guard shared
    // between this add, the generation job that writes into it and, once
    // the node is published, the stack entity; the last owner removes it.
    let (working_dir, excluded_dirs) =
        copy_node_to_temp_dir(&source_path, &ctx.action.peppy_dirs.tmp_dir())
            .map_err(|e| format!("Failed to copy node folder: {}", e))?;
    let staging = Arc::new(WorkingDirGuard::new(working_dir));

    if !excluded_dirs.is_empty() {
        ctx.announcer.line(format!(
            "Excluded directories from copy: {}",
            excluded_dirs.join(", ")
        ));
    }

    // Presence pass, before build_cmd so a wrong stack fails fast: a
    // dependency may be absent only where its slot admits the empty set.
    let dep_errors = validate_dependency_specs(
        &node_config.manifest,
        &node_config.interfaces,
        &node_name,
        &node_tag,
        MissingDependencyPolicy::AllowAbsentWhenSlotAdmitsEmpty,
        stack_resolver(&ctx.action.node_stack),
    );
    if let Some(err) = dep_errors.into_iter().next() {
        return Err(AddStepFailure::new(
            PeppyFailureKind::PreconditionFailed,
            format!("Failed to add node config: {}", err),
        ));
    }

    // Resolvability pass: an admitted-absent dependency still feeds the
    // consumed modules peppygen generates, so it must resolve through the
    // repository cache instead, and exposure is re-checked against the exact
    // configs the generator consumes.
    let MaterializedDeps {
        configs: repo_resolved,
        provenance: repo_provenance,
        ..
    } = materialize_repo_deps(
        &node_config.manifest,
        &ctx.action.node_stack,
        &ctx.action.peppy_dirs,
        DepClosure::Direct,
    )
    .await
    .map_err(|reason| format!("Failed to add node config: {}", reason))?;
    for entry in &repo_provenance {
        ctx.announcer.line(format!(
            "Resolved dependency `{}:{}` from the repository cache ({})",
            entry.name,
            entry.tag,
            entry.source_kind.as_str()
        ));
    }
    let resolve_dep = stack_then_repo_resolver(&ctx.action.node_stack, &repo_resolved);
    let dep_errors = validate_dependency_specs(
        &node_config.manifest,
        &node_config.interfaces,
        &node_name,
        &node_tag,
        MissingDependencyPolicy::RequireResolvable,
        &resolve_dep,
    );
    if let Some(err) = dep_errors.into_iter().next() {
        return Err(AddStepFailure::new(
            PeppyFailureKind::PreconditionFailed,
            format!("Failed to add node config: {}", err),
        ));
    }

    // Generate the peppygen library in the working directory.
    // Container builds need Copy mode because Apptainer's `%files` copies symlinks
    // as-is; absolute symlinks to the host cache would be broken inside the container.
    let language = node_config.execution.language;
    let deploy_mode = if node_config.execution.container.is_some() {
        generator::CrateDeployMode::Copy
    } else {
        generator::CrateDeployMode::Symlink
    };
    let interface_feedback = interface_step_sink(ctx.announcer.clone());
    // A goal carrying pins fixes which bytes its contract and pairing
    // documents are: the launch (or the resolve-ref lowering) decided them
    // once, and this machine's own cache picks must not override that.
    // A goal without pins keeps the local resolution rules.
    let doc_pins = if goal.pins_json5.is_empty() {
        None
    } else {
        let decoded = super::pins::decode_pins(&goal.pins_json5)?;
        Some(super::pins::DocPins::from_pins(&decoded))
    };
    let consumed_interfaces = collect_all_deployment_interfaces(
        &node_config.manifest,
        &node_config.interfaces,
        &resolve_dep,
        &ctx.action.peppy_dirs,
        doc_pins.as_ref(),
        &interface_feedback,
    )?;
    // Generation is synchronous and runs on the blocking pool, holding its
    // own share of the staging directory (see `staged`). The working dir is
    // a copy staged for this build: generated code must carry no path into
    // it, or byte-identical sources would fingerprint differently on every
    // add and never reuse a cached artifact.
    let generation = StagedJob {
        node_label: format!("{node_name}:{node_tag}"),
        log_path: ctx.announcer.log().path(),
        staging: Arc::clone(&staging),
    };
    let git_hash = goal.git_hash.clone();
    let peppy_dirs = ctx.action.peppy_dirs.clone();
    run_staged_job(generation, move |staging_dir| {
        generator::generate_peppygen_lib(
            language,
            staging_dir,
            consumed_interfaces,
            &git_hash,
            &peppy_dirs,
            deploy_mode,
            None,
            generator::NodeTree::Staged,
        )
        .map_err(|e| format!("Failed to generate peppygen library: {}", e))
    })
    .await?;

    // Stop any pre-existing instances of this node before pushing the new
    // config. `push_config` rejects replacements that still have live
    // instances (it would otherwise orphan them), so we shut them down
    // first to satisfy that precondition.
    // The one failure of the shutdown is a start in flight, which the
    // caller waits out.
    shutdown_existing_instances(&node_name, &node_tag, ctx)
        .await
        .map_err(|refusal| {
            AddStepFailure::new(
                PeppyFailureKind::PreconditionFailed,
                format!("Failed to shutdown existing node instances: {refusal}"),
            )
        })?;

    // Push the node config into the stack as an `Added` entity. Use the
    // staged copy of peppy.json5 rather than source_path because
    // source_path may point at a transient Git/Http clone that is cleaned
    // up after this function returns. The staging directory persists as
    // long as the entity holds its guard.
    let config_path_for_stack = staging.path().join(NODE_CONFIG_FILE);
    ctx.action
        .node_stack
        .push_config(node_config.clone(), false, &config_path_for_stack)
        .map_err(|error| {
            AddStepFailure::new(
                push_failure_kind(&error),
                format!("Failed to add node config: {error}"),
            )
        })?;

    let entity_handle = ctx
        .action
        .node_stack
        .find(&node_name, &node_tag)
        .ok_or_else(|| {
            format!(
                "internal error: just-pushed entity {}:{} disappeared from the stack",
                node_name, node_tag
            )
        })?;

    // Hand the staging directory to the entity so a follow-up `node_build`
    // can reuse it without re-cloning the source. The entity's guard is the
    // one generation ran under; the directory is removed when the entity
    // lets go of it.
    {
        let mut guard = entity_handle.write();
        ctx.action
            .node_stack
            .set_add_log_path(&node_name, &node_tag, ctx.announcer.log().path());
        guard.set_pending_working_dir(Arc::clone(&staging));
    }

    debug!("Added node {}:{} (pending build)", node_name, node_tag);

    Ok(NodeAddResult::new(node_name, node_tag))
}

/// The kind of a failed `push_config`: the root node is not a node a goal
/// can name, and a node with live instances or with dependents is in a
/// state that forbids the overwrite.
fn push_failure_kind(error: &NodeStackError) -> PeppyFailureKind {
    match error {
        NodeStackError::CannotModifyRootNode => PeppyFailureKind::Rejected,
        NodeStackError::CannotOverwriteNodeWithLiveInstances { .. }
        | NodeStackError::CannotOverwriteNodeWithDependents { .. } => {
            PeppyFailureKind::PreconditionFailed
        }
        _ => PeppyFailureKind::AttemptFailed,
    }
}

/// The failure of one step of [`process_node_add_inner`]: its kind and its
/// message.
struct AddStepFailure {
    kind: PeppyFailureKind,
    message: String,
}

impl AddStepFailure {
    fn new(kind: PeppyFailureKind, message: String) -> Self {
        Self { kind, message }
    }
}

/// A step that fails with a plain message is an attempt that gave no
/// result.
impl From<String> for AddStepFailure {
    fn from(message: String) -> Self {
        Self::new(PeppyFailureKind::AttemptFailed, message)
    }
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

    #[test]
    fn test_is_transient_http_error_classification() {
        assert!(is_transient_http_error(&HttpError::StatusCode(500)));
        assert!(is_transient_http_error(&HttpError::StatusCode(503)));
        assert!(is_transient_http_error(&HttpError::ConnectionFailed));
        assert!(is_transient_http_error(&HttpError::Io(
            std::io::Error::other("reset")
        )));
        assert!(!is_transient_http_error(&HttpError::StatusCode(404)));
        assert!(!is_transient_http_error(&HttpError::StatusCode(403)));
        assert!(!is_transient_http_error(&HttpError::TooManyRedirects));
    }

    #[test]
    fn test_download_http_bundle_retries_5xx_then_succeeds() {
        let bundle = create_test_tar_zst(b"hello world");
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/bundle.tar.zst"))
                .times(3)
                .respond_with(httptest::responders::cycle![
                    status_code(503),
                    status_code(503),
                    status_code(200).body(bundle),
                ]),
        );

        let url = url::Url::parse(&server.url("/bundle.tar.zst").to_string()).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("bundle.tar.zst");

        let result = download_http_bundle(&url, &dest, None, None);
        assert!(result.is_ok(), "expected Ok after retries, got: {result:?}");
        assert!(dest.exists(), "bundle file should exist");
    }

    #[test]
    fn test_download_http_bundle_does_not_retry_4xx() {
        let server = Server::run();
        // `times(1)` makes the server itself assert (on drop) that no
        // retry request ever arrived.
        server.expect(
            Expectation::matching(request::method_path("GET", "/bundle.tar.zst"))
                .times(1)
                .respond_with(status_code(404)),
        );

        let url = url::Url::parse(&server.url("/bundle.tar.zst").to_string()).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("bundle.tar.zst");

        let err = download_http_bundle(&url, &dest, None, None).unwrap_err();
        assert!(err.contains("404"), "error should carry the status: {err}");
    }

    #[test]
    fn node_config_path_names_a_json5_file_or_a_directory() {
        let cases = [
            ("nodes/camera", "nodes/camera/peppy.json5"),
            ("nodes/camera/custom.json5", "nodes/camera/custom.json5"),
            ("nodes/camera/CUSTOM.JSON5", "nodes/camera/CUSTOM.JSON5"),
            ("nodes/camera.json", "nodes/camera.json/peppy.json5"),
        ];
        for (repo_path, expected) in cases {
            assert_eq!(
                node_config_path(Path::new(repo_path)),
                PathBuf::from(expected),
                "repo path {repo_path}"
            );
        }
    }

    #[test]
    fn download_progress_line_formats_known_and_unknown_totals() {
        let cases = [
            (512, None, "Downloading: 512 B"),
            (1, Some(4), "Downloading: 1 B / 4 B (25%)"),
            (7, Some(0), "Downloading: 7 B / 0 B (0%)"),
            (
                3 * 1024 * 1024 / 2,
                Some(3 * 1024 * 1024),
                "Downloading: 1.5 MB / 3.0 MB (50%)",
            ),
        ];
        for (downloaded, total, expected) in cases {
            assert_eq!(download_progress_line(downloaded, total), expected);
        }
    }

    #[test]
    fn decode_sha256_hex_refuses_non_hex_digits() {
        let err = decode_sha256_hex(&"z".repeat(64)).unwrap_err();
        assert!(err.contains("not a hexadecimal string"), "{err}");
    }

    #[test]
    fn decode_sha256_hex_accepts_either_case() {
        let lower = "0123456789abcdef".repeat(4);
        let decoded = decode_sha256_hex(&lower).expect("lowercase hex decodes");
        assert_eq!(decoded.len(), 32);
        assert_eq!(
            &decoded[..8],
            &[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]
        );
        assert_eq!(
            decode_sha256_hex(&lower.to_uppercase()).expect("uppercase hex decodes"),
            decoded
        );
    }

    /// The digest is decoded before any request is sent, so a malformed one
    /// is refused without downloading anything.
    #[test]
    fn download_http_bundle_refuses_a_non_hex_sha256() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/bundle.tar.zst"))
                .times(0)
                .respond_with(status_code(200)),
        );

        let url = url::Url::parse(&server.url("/bundle.tar.zst").to_string()).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("bundle.tar.zst");

        let err = download_http_bundle(&url, &dest, Some(&"g".repeat(64)), None).unwrap_err();
        assert!(err.contains("Invalid SHA256"), "{err}");
        assert!(
            !dest.exists(),
            "nothing is downloaded for a malformed digest"
        );
    }
}
