use super::super::stack::STACK_LAUNCH_GIT_HASH;
use super::sync::{collect_subscribed_interfaces, generate_peppygen_for_node};
use crate::Result;
use crate::encoding::{
    NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse, NodeAddResult, NodeSource,
};
use crate::names;
use bytes::Bytes;
use chrono::Local;
use config::consts::{
    NODE_CONFIG_FILE, PEPPY_OUTPUT_DIR, PEPPYGEN_OUTPUT_PATH, logs_dir_add, peppy_data_dir,
};
use config::node::{NodeConfig, NodeConfigParser};
use git2::{Repository, build::CheckoutBuilder};
use node_stack::{NodeStack, validate_dependency_specs};
use peppylib::messaging::{
    ActionCreation, SHUTDOWN_SERVICE, ServiceMessenger, ServiceRequestContext, TopicPublisher,
};
use peppylib::{ActionMessenger, MessengerHandle, PeppyResult};
use rand::Rng;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tar::Archive;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tracing::debug;
use ureq::Error as HttpError;
use zstd::stream::read::Decoder;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn listen_for_node_add(
    messenger: &MessengerHandle,
    master_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    let action = ActionMessenger::expose(
        messenger,
        master_node_name,
        instance_id,
        node_name,
        names::NODE_ADD_ACTION,
    )
    .await?;

    let handle = tokio::spawn({
        let messenger = messenger.clone();
        let bound_master_node = master_node_name.to_string();
        let master_instance_id = instance_id.to_string();
        async move {
            run_node_add_action_loop(
                action,
                node_stack,
                messenger,
                bound_master_node,
                master_instance_id,
            )
            .await
        }
    });

    Ok(handle)
}

fn generate_random_id() -> String {
    let mut rng = rand::rng();
    let bytes: [u8; 3] = rng.random();
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;

    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_name = entry.file_name();

        // Skip .peppy directory
        if file_name == ".peppy" {
            continue;
        }

        let src_path = entry.path();
        let dest_path = dest.join(file_name);

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dest_path)?;
        } else {
            std::fs::copy(&src_path, &dest_path)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum FeedbackStream {
    Stdout,
    Stderr,
}

struct FeedbackLine {
    stream: FeedbackStream,
    line: String,
}

fn spawn_output_reader<R: Read + Send + 'static>(
    reader: R,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    stream: FeedbackStream,
    log_file: Arc<StdMutex<File>>,
) -> JoinHandle<()> {
    let stream_prefix = match stream {
        FeedbackStream::Stdout => "stdout",
        FeedbackStream::Stderr => "stderr",
    };

    tokio::task::spawn_blocking(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines().map_while(|r| r.ok()) {
            // Always write to log file
            if let Ok(mut file) = log_file.lock() {
                let timestamp = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
                let _ = writeln!(file, "[{}] [{}] {}", timestamp, stream_prefix, line);
            }

            let _ = feedback_tx.send(FeedbackLine {
                stream,
                line: line.to_string(),
            });
        }
    })
}

/// Runs the add_cmd for a node and streams output via the feedback publisher.
/// Returns Ok(()) if add_cmd is None or executes successfully.
async fn run_add_cmd_with_streaming(
    add_cmd: Option<&Vec<String>>,
    working_dir: &Path,
    env_vars: &[(String, String)],
    feedback_publisher: &TopicPublisher,
    log_file: Arc<StdMutex<File>>,
) -> std::result::Result<(), String> {
    let Some(cmd) = add_cmd else {
        return Ok(());
    };

    if cmd.is_empty() {
        return Err("add_cmd is empty".to_string());
    };

    let (program, args) = if cmd.len() == 1 {
        if cfg!(windows) {
            ("cmd".to_string(), vec!["/C".to_string(), cmd[0].clone()])
        } else {
            ("sh".to_string(), vec!["-c".to_string(), cmd[0].clone()])
        }
    } else {
        (cmd[0].clone(), cmd[1..].to_vec())
    };

    debug!(
        "Running add_cmd: {} {:?} in dir {:?}",
        program, args, working_dir
    );

    // Log the command being executed to the log file before attempting to spawn
    {
        let full_cmd = std::iter::once(program.as_str())
            .chain(args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        if let Ok(mut file) = log_file.lock() {
            let timestamp = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
            let _ = writeln!(
                file,
                "[{}] Executing add_cmd: {} (working_dir: {})",
                timestamp,
                full_cmd,
                working_dir.display()
            );
            let _ = file.flush();
        }
    }

    let mut command = Command::new(&program);
    command.args(&args);
    command.current_dir(working_dir);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    for (key, value) in env_vars {
        command.env(key, value);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("failed to execute add_cmd: {}", e))?;

    let (feedback_tx, mut feedback_rx) = mpsc::unbounded_channel::<FeedbackLine>();
    let feedback_publisher = feedback_publisher.clone();
    let publisher_handle = tokio::spawn(async move {
        while let Some(line) = feedback_rx.recv().await {
            let feedback = match line.stream {
                FeedbackStream::Stdout => NodeAddFeedback::stdout(&line.line),
                FeedbackStream::Stderr => NodeAddFeedback::stderr(&line.line),
            };
            if let Ok(payload) = feedback.encode() {
                let _ = feedback_publisher.publish(payload).await;
            }
        }
    });

    let mut reader_handles = Vec::new();

    // Stream stdout
    if let Some(stdout) = child.stdout.take() {
        reader_handles.push(spawn_output_reader(
            stdout,
            feedback_tx.clone(),
            FeedbackStream::Stdout,
            Arc::clone(&log_file),
        ));
    }

    // Stream stderr
    if let Some(stderr) = child.stderr.take() {
        reader_handles.push(spawn_output_reader(
            stderr,
            feedback_tx.clone(),
            FeedbackStream::Stderr,
            Arc::clone(&log_file),
        ));
    }

    // Wait for the process to complete
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .map_err(|e| format!("failed to wait for add_cmd: {}", e))?
        .map_err(|e| format!("failed to wait for add_cmd: {}", e))?;

    for handle in reader_handles {
        let _ = handle.await;
    }
    drop(feedback_tx);
    let _ = publisher_handle.await;

    if !status.success() {
        return Err(format!("add_cmd failed with status {}", status));
    }

    debug!("add_cmd completed successfully");
    Ok(())
}

/// Copies a node folder to the peppy nodes storage directory.
///
/// The destination path follows the format: `<storage_dir>/<node_name>_<tag>_<uuid>`
///
/// Returns the path to the copied folder.
fn copy_node_to_storage(from_dir: &Path, node_name: &str, node_tag: &str) -> Result<PathBuf> {
    let storage_dir = peppy_data_dir().join("nodes");
    let random_id = generate_random_id();
    let folder_name = format!("{}_{}_{}", node_name, node_tag, random_id);
    let dest_path = storage_dir.join(&folder_name);

    debug!(
        "Copying node folder from {} to {}",
        from_dir.display(),
        dest_path.display()
    );

    copy_dir_recursive(from_dir, &dest_path)?;

    Ok(dest_path)
}

/// Verifies that the node directory is in sync with the currently running daemon
/// by comparing the stored git hash with the expected one.
///
/// Returns `Ok(())` if verification passes, or `Err(error_message)` if it fails.
fn verify_git_hash(source_path: &Path, expected_git_hash: &str) -> std::result::Result<(), String> {
    let git_hash_path = source_path.join(PEPPY_OUTPUT_DIR).join("git.hash");
    let stored_git_hash = std::fs::read_to_string(&git_hash_path).map_err(|e| {
        format!(
            "Missing required git hash file at {}: {}. Run `peppy node sync` before `peppy node add`.",
            git_hash_path.display(),
            e
        )
    })?;

    let expected_git_hash = expected_git_hash.trim();
    let stored_git_hash = stored_git_hash.trim();

    if stored_git_hash.is_empty() {
        return Err(format!(
            "Invalid git hash file at {}: file is empty. Run `peppy node sync` before `peppy node add`.",
            git_hash_path.display(),
        ));
    }

    if stored_git_hash != expected_git_hash {
        return Err(format!(
            "git hash mismatch for node directory {} (expected '{}', found '{}' in {}). Run `peppy node sync` before retrying.",
            source_path.display(),
            expected_git_hash,
            stored_git_hash,
            git_hash_path.display(),
        ));
    }

    Ok(())
}

fn is_node_snapshot_path(path: &Path, node_name: &str, node_tag: &str) -> bool {
    let storage_dir = peppy_data_dir().join("nodes");
    if !path.starts_with(&storage_dir) {
        return false;
    }
    let Some(folder_name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    folder_name.starts_with(&format!("{node_name}_{node_tag}_"))
}

/// State for tracking the current node add action.
#[derive(Default)]
enum NodeAddActionState {
    /// No action is currently running.
    #[default]
    Idle,
    /// The goal was rejected (no result polling expected).
    Rejected,
    /// An action is currently running.
    Running {
        started_at: Instant,
        timeout_secs: u64,
    },
    /// The action completed and the result is ready to be sent.
    Completed { result: NodeAddResult },
    /// The result has been sent to the requester.
    ResultSent { result: NodeAddResult },
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
    verify_codegen_fingerprint: bool,
    cleanup_dir: Option<PathBuf>,
}

struct ProcessNodeAddContext {
    messenger: MessengerHandle,
    bound_master_node: String,
    master_instance_id: String,
    node_stack: Arc<NodeStack>,
    feedback_publisher: TopicPublisher,
    log_file: Arc<StdMutex<File>>,
    log_path: PathBuf,
}

fn sanitize_repo_path(repo_path: &str) -> std::result::Result<PathBuf, String> {
    use std::path::Component;

    let trimmed = repo_path.trim_start_matches(['/', '\\']);
    let path = PathBuf::from(trimmed);

    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("repo_path must not contain '..'".to_string());
    }

    Ok(path)
}

fn checkout_repo_ref(repo: &Repository, repo_ref: &str) -> std::result::Result<(), git2::Error> {
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

async fn resolve_git_source(
    repo_url: &gix_url::Url,
    repo_path: &str,
    repo_ref: Option<&str>,
) -> std::result::Result<ResolvedNodeAddSource, String> {
    let repo_relative_path = sanitize_repo_path(repo_path)?;

    let checkout_dir = tempfile::tempdir()
        .map_err(|e| format!("Failed to create temporary directory: {}", e))?
        .keep();

    let repo_url_bstring = repo_url.to_bstring();
    let repo_url_str = std::str::from_utf8(repo_url_bstring.as_ref())
        .map_err(|_| "repo_url must be valid UTF-8".to_string())?
        .to_owned();

    let clone_checkout_dir = checkout_dir.clone();
    let clone_repo_url = repo_url_str.clone();
    let clone_repo_ref = repo_ref.map(str::to_owned);
    if let Err(err) = tokio::task::spawn_blocking(move || {
        let repo = Repository::clone(&clone_repo_url, &clone_checkout_dir)
            .map_err(|e| format!("Failed to clone repository: {}", e))?;
        if let Some(repo_ref) = clone_repo_ref.as_deref() {
            checkout_repo_ref(&repo, repo_ref)
                .map_err(|e| format!("Failed to checkout git ref '{}': {}", repo_ref, e))?;
        }
        Ok::<_, String>(())
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

    let node_config = match NodeConfigParser::from_path(&config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            std::fs::remove_dir_all(&checkout_dir).ok();
            return Err(format!(
                "Failed to parse node config at {}: {}",
                config_path.display(),
                e
            ));
        }
    };

    Ok(ResolvedNodeAddSource {
        source_path: node_root_dir,
        node_config,
        verify_codegen_fingerprint: false,
        cleanup_dir: Some(checkout_dir),
    })
}

fn is_supported_http_archive(url: &url::Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    path.ends_with(".tar.zst") || path.ends_with(".tar.zstd") || path.ends_with(".tzst")
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

fn download_http_bundle(url: &url::Url, destination: &Path) -> std::result::Result<(), String> {
    let response = ureq::get(url.as_str()).call().map_err(|err| {
        let reason = match err {
            HttpError::StatusCode(code) => format!("unexpected status code {code}"),
            other => other.to_string(),
        };
        format!("Failed to download bundle from {}: {}", url, reason)
    })?;

    let mut reader = response.into_body().into_reader();
    let mut file = File::create(destination).map_err(|e| {
        format!(
            "Failed to create bundle file {}: {}",
            destination.display(),
            e
        )
    })?;

    let mut buffer = [0u8; 8 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|e| format!("Failed to read response body from {}: {}", url, e))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read]).map_err(|e| {
            format!(
                "Failed to write bundle file {}: {}",
                destination.display(),
                e
            )
        })?;
    }
    file.flush().map_err(|e| {
        format!(
            "Failed to flush bundle file {}: {}",
            destination.display(),
            e
        )
    })?;

    Ok(())
}

fn extract_http_bundle(
    bundle_path: &Path,
    destination: &Path,
    url: &url::Url,
) -> std::result::Result<(), String> {
    let file = File::open(bundle_path).map_err(|e| {
        format!(
            "Failed to open downloaded bundle {}: {}",
            bundle_path.display(),
            e
        )
    })?;

    let decoder = Decoder::new(file)
        .map_err(|e| format!("Failed to decode zstd bundle from {}: {}", url, e))?;
    let mut archive = Archive::new(decoder);

    let entries = archive
        .entries()
        .map_err(|e| format!("Failed to read tar bundle entries from {}: {}", url, e))?;

    let mut directories = Vec::new();
    for entry in entries {
        let mut entry =
            entry.map_err(|e| format!("Failed to read tar bundle entry from {}: {}", url, e))?;

        let entry_path = entry
            .path()
            .map_err(|e| format!("Failed to read tar bundle entry path from {}: {}", url, e))?
            .into_owned();

        if entry_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(..)
            )
        }) {
            return Err(format!(
                "Bundle from {} contains unsafe path: {}",
                url,
                entry_path.display()
            ));
        }

        if entry.header().entry_type().is_dir() {
            directories.push(entry);
        } else {
            let unpacked = entry.unpack_in(destination).map_err(|e| {
                format!(
                    "Failed to unpack tar bundle entry {} from {}: {}",
                    entry_path.display(),
                    url,
                    e
                )
            })?;
            if !unpacked {
                return Err(format!(
                    "Bundle from {} contains unsafe path: {}",
                    url,
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
            .map_err(|e| format!("Failed to read tar bundle entry path from {}: {}", url, e))?
            .into_owned();
        let unpacked = dir.unpack_in(destination).map_err(|e| {
            format!(
                "Failed to unpack tar bundle entry {} from {}: {}",
                entry_path.display(),
                url,
                e
            )
        })?;
        if !unpacked {
            return Err(format!(
                "Bundle from {} contains unsafe path: {}",
                url,
                entry_path.display()
            ));
        }
    }

    Ok(())
}

fn locate_node_root_dir(extracted_dir: &Path) -> std::result::Result<PathBuf, String> {
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

async fn resolve_http_source(url: &url::Url) -> std::result::Result<ResolvedNodeAddSource, String> {
    let url = url.clone();
    tokio::task::spawn_blocking(move || resolve_http_source_blocking(url))
        .await
        .map_err(|e| format!("Failed to join HTTP download task: {}", e))?
}

fn resolve_http_source_blocking(
    url: url::Url,
) -> std::result::Result<ResolvedNodeAddSource, String> {
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

    let http_base_dir = peppy_data_dir().join("http_downloads");
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

    download_http_bundle(&url, &bundle_path)?;
    extract_http_bundle(&bundle_path, &extract_dir, &url)?;
    std::fs::remove_file(&bundle_path).ok();

    let node_root_dir = locate_node_root_dir(&extract_dir)?;
    let config_path = node_root_dir.join(NODE_CONFIG_FILE);
    let node_config = NodeConfigParser::from_path(&config_path).map_err(|e| {
        format!(
            "Failed to parse node config at {}: {}",
            config_path.display(),
            e
        )
    })?;

    let operation_dir = operation_cleanup.take();

    Ok(ResolvedNodeAddSource {
        source_path: node_root_dir,
        node_config,
        verify_codegen_fingerprint: false,
        cleanup_dir: operation_dir,
    })
}

async fn resolve_node_add_source(
    goal: &NodeAddGoal,
) -> std::result::Result<ResolvedNodeAddSource, String> {
    match &goal.source {
        NodeSource::Fs(path) => {
            // When using the stack-launch marker, skip git hash verification and fingerprint
            // checks. This allows stack_launch to work with local filesystem sources without
            // requiring `peppy node sync` beforehand - fresh peppygen will be generated.
            let is_stack_launch = goal.git_hash == STACK_LAUNCH_GIT_HASH;
            if !is_stack_launch {
                verify_git_hash(path, &goal.git_hash)?;
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
                verify_codegen_fingerprint: !is_stack_launch,
                cleanup_dir: None,
            })
        }
        NodeSource::Git {
            repo_url,
            repo_path,
            repo_ref,
        } => resolve_git_source(repo_url, repo_path, repo_ref.as_deref()).await,
        NodeSource::Http { url } => resolve_http_source(url).await,
    }
}

async fn run_node_add_action_loop(
    mut action: ActionCreation,
    node_stack: Arc<NodeStack>,
    messenger: MessengerHandle,
    bound_master_node: String,
    master_instance_id: String,
) -> Result<()> {
    let state = Arc::new(Mutex::new(NodeAddActionState::default()));

    loop {
        // Wait for a goal request
        let goal_result = action
            .goal_service
            .handle_next_request({
                let feedback_publisher = &action.feedback_publisher;
                let node_stack = Arc::clone(&node_stack);
                let state = Arc::clone(&state);
                let messenger = messenger.clone();
                let bound_master_node = bound_master_node.clone();
                let master_instance_id = master_instance_id.clone();
                move |context| {
                    let feedback_publisher = feedback_publisher.clone();
                    let node_stack = Arc::clone(&node_stack);
                    let state = Arc::clone(&state);
                    let messenger = messenger.clone();
                    let bound_master_node = bound_master_node.clone();
                    let master_instance_id = master_instance_id.clone();

                    async move {
                        handle_goal_request(
                            context,
                            feedback_publisher,
                            node_stack,
                            state,
                            messenger,
                            bound_master_node,
                            master_instance_id,
                        )
                        .await
                    }
                }
            })
            .await;

        match goal_result {
            Ok(true) => {
                // Check if the goal was rejected (no result polling expected)
                {
                    let mut state_guard = state.lock().await;
                    if matches!(*state_guard, NodeAddActionState::Rejected) {
                        // Goal was rejected, reset to Idle and wait for next goal
                        *state_guard = NodeAddActionState::Idle;
                        continue;
                    }
                }

                // Goal accepted, now wait for result, cancel, or new goal requests.
                // We also listen for new goals here to handle the case where a client
                // abandons an action without polling for the result.
                loop {
                    tokio::select! {
                        cancel_result = action.cancel_service.handle_next_request({
                            let state = Arc::clone(&state);
                            move |context| {
                                let state = Arc::clone(&state);
                                async move { handle_cancel_request(context, state).await }
                            }
                        }) => {
                            match cancel_result {
                                Ok(true) => {}
                                Ok(false) => return Ok(()),
                                Err(e) => {
                                    debug!("Cancel service error: {}", e);
                                    return Err(e.into());
                                }
                            }
                        }
                        result_result = action.result_service.handle_next_request({
                            let state = Arc::clone(&state);
                            move |context| {
                                let state = Arc::clone(&state);
                                async move { handle_result_request(context, state).await }
                            }
                        }) => {
                            match result_result {
                                Ok(true) => {
                                    // Only reset and accept a new goal after we've delivered the final result.
                                    let mut state_guard = state.lock().await;
                                    if matches!(*state_guard, NodeAddActionState::ResultSent { .. }) {
                                        *state_guard = NodeAddActionState::default();
                                        break;
                                    }
                                }
                                Ok(false) => return Ok(()),
                                Err(e) => {
                                    debug!("Result service error: {}", e);
                                    return Err(e.into());
                                }
                            }
                        }
                        // Handle new goals while waiting for result/cancel.
                        // This allows a new goal to be accepted if the previous action
                        // completed but the client never polled for the result.
                        // If an action is still running, the goal will be rejected
                        // with "action already in progress".
                        goal_result = action.goal_service.handle_next_request({
                            let feedback_publisher = &action.feedback_publisher;
                            let node_stack = Arc::clone(&node_stack);
                            let state = Arc::clone(&state);
                            let messenger = messenger.clone();
                            let bound_master_node = bound_master_node.clone();
                            let master_instance_id = master_instance_id.clone();
                            move |context| {
                                let feedback_publisher = feedback_publisher.clone();
                                let node_stack = Arc::clone(&node_stack);
                                let state = Arc::clone(&state);
                                let messenger = messenger.clone();
                                let bound_master_node = bound_master_node.clone();
                                let master_instance_id = master_instance_id.clone();
                                async move {
                                    handle_goal_request(
                                        context,
                                        feedback_publisher,
                                        node_stack,
                                        state,
                                        messenger,
                                        bound_master_node,
                                        master_instance_id,
                                    )
                                    .await
                                }
                            }
                        }) => {
                            match goal_result {
                                Ok(true) => {
                                    // Goal was handled. Check if it was rejected.
                                    let mut state_guard = state.lock().await;
                                    if matches!(*state_guard, NodeAddActionState::Rejected) {
                                        // Goal was rejected (for reasons other than "in progress"),
                                        // reset to Idle and continue waiting.
                                        *state_guard = NodeAddActionState::Idle;
                                    }
                                    // If state is Running: either old action is still running
                                    // (new goal rejected for "in progress"), or new goal was
                                    // accepted and a new action started. Either way, continue
                                    // waiting in the inner loop.
                                }
                                Ok(false) => return Ok(()),
                                Err(e) => {
                                    debug!("Goal service error: {}", e);
                                    return Err(e.into());
                                }
                            }
                        }
                    }
                }
            }
            Ok(false) => {
                debug!("Goal service closed");
                return Ok(());
            }
            Err(e) => {
                debug!("Goal service error: {}", e);
                return Err(e.into());
            }
        }
    }
}

async fn handle_goal_request(
    context: ServiceRequestContext,
    feedback_publisher: TopicPublisher,
    node_stack: Arc<NodeStack>,
    state: Arc<Mutex<NodeAddActionState>>,
    messenger: MessengerHandle,
    bound_master_node: String,
    master_instance_id: String,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let goal = match NodeAddGoal::decode(&payload.as_bytes()) {
        Ok(g) => g,
        Err(e) => {
            let response = NodeAddGoalResponse::rejected(format!("invalid payload: {}", e));
            return response
                .encode()
                .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                    identifier: "node_add_goal".to_string(),
                    reason: format!("Failed to encode response: {}", e),
                });
        }
    };

    // Check if already running and mark as running if not
    {
        let mut state_guard = state.lock().await;
        if let NodeAddActionState::Running {
            started_at,
            timeout_secs,
        } = *state_guard
        {
            let remaining = Duration::from_secs(timeout_secs)
                .saturating_sub(started_at.elapsed())
                .as_secs();
            let response = NodeAddGoalResponse::rejected(format!(
                "action already in progress (times out in {remaining}s)"
            ));
            return response
                .encode()
                .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                    identifier: "node_add_goal".to_string(),
                    reason: format!("Failed to encode response: {}", e),
                });
        }
        *state_guard = NodeAddActionState::Running {
            started_at: Instant::now(),
            timeout_secs: goal.timeout_secs,
        };
    }

    let mut resolved = match resolve_node_add_source(&goal).await {
        Ok(resolved) => resolved,
        Err(error_msg) => {
            let mut state_guard = state.lock().await;
            *state_guard = NodeAddActionState::Rejected;
            let response = NodeAddGoalResponse::rejected(&error_msg);
            return response
                .encode()
                .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                    identifier: "node_add_goal".to_string(),
                    reason: format!("Failed to encode response: {}", e),
                });
        }
    };

    let mut checkout_cleanup = CleanupDir::new(resolved.cleanup_dir.take());

    match &goal.source {
        NodeSource::Fs(_) => debug!(
            "Received `node_add` goal from {sender_instance_id}, source={}",
            resolved.source_path.display()
        ),
        NodeSource::Git {
            repo_url,
            repo_path,
            repo_ref,
        } => debug!(
            "Received `node_add` goal from {sender_instance_id}, source=git:{}::{} ({:?})",
            repo_url, repo_path, repo_ref
        ),
        NodeSource::Http { url } => debug!(
            "Received `node_add` goal from {sender_instance_id}, source=http:{}",
            url
        ),
    }

    let node_name = resolved.node_config.manifest.name.as_str().to_owned();
    let node_tag = resolved.node_config.manifest.tag.clone();

    // Create log file with timestamp-based filename
    let log_dir = logs_dir_add();
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        let error_msg = format!("Failed to create logs directory: {}", e);
        debug!("Failed to create logs directory {:?}: {}", log_dir, e);
        let mut state_guard = state.lock().await;
        *state_guard = NodeAddActionState::Rejected;
        let response = NodeAddGoalResponse::rejected(&error_msg);
        return response
            .encode()
            .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                identifier: "node_add_goal".to_string(),
                reason: format!("Failed to encode response: {}", e),
            });
    }

    let timestamp = Local::now().format("%Y%m%d_%H%M%S_%3f");
    let log_filename = format!("{}_{}_{}.log", node_name, node_tag, timestamp);
    let log_path = log_dir.join(&log_filename);
    let log_file = match File::create(&log_path) {
        Ok(file) => Arc::new(StdMutex::new(file)),
        Err(e) => {
            let error_msg = format!("Failed to create log file: {}", e);
            debug!("Failed to create log file {:?}: {}", log_path, e);
            let mut state_guard = state.lock().await;
            *state_guard = NodeAddActionState::Rejected;
            let response = NodeAddGoalResponse::rejected(&error_msg);
            return response
                .encode()
                .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                    identifier: "node_add_goal".to_string(),
                    reason: format!("Failed to encode response: {}", e),
                });
        }
    };

    debug!("Created log file for node add: {}", log_path.display());

    // Process the add operation in a separate task to not block goal response
    let state_clone = Arc::clone(&state);
    let feedback_publisher_clone = feedback_publisher.clone();
    let log_path_clone = log_path.clone();
    let source_path = resolved.source_path.clone();
    let verify_codegen_fingerprint = resolved.verify_codegen_fingerprint;
    let cleanup_dir = checkout_cleanup.take();
    let node_config = resolved.node_config;
    tokio::spawn(async move {
        let ctx = ProcessNodeAddContext {
            messenger,
            bound_master_node,
            master_instance_id,
            node_stack,
            feedback_publisher: feedback_publisher_clone,
            log_file,
            log_path: log_path_clone,
        };
        let result = process_node_add(
            goal,
            node_config,
            source_path,
            verify_codegen_fingerprint,
            cleanup_dir,
            ctx,
        )
        .await;
        let mut state_guard = state_clone.lock().await;
        *state_guard = NodeAddActionState::Completed { result };
    });

    let response = NodeAddGoalResponse::accepted(&log_path);
    response
        .encode()
        .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
            identifier: "node_add_goal".to_string(),
            reason: format!("Failed to encode response: {}", e),
        })
}

async fn shutdown_existing_instances(
    node_name: &str,
    node_tag: &str,
    ctx: &ProcessNodeAddContext,
) -> std::result::Result<(), String> {
    let Some(entity) = ctx.node_stack.find(node_name, node_tag) else {
        return Ok(());
    };

    if entity.instances().is_empty() {
        return Ok(());
    }

    if !ctx.node_stack.dependents_of(node_name, node_tag).is_empty() {
        let err = node_stack::NodeStackError::CannotOverwriteNodeWithDependents {
            node_name: node_name.to_string(),
            node_tag: node_tag.to_string(),
        };
        return Err(err.to_string());
    }

    let instances = entity
        .instances()
        .iter()
        .map(|instance| instance.instance_id().clone())
        .collect::<Vec<_>>();

    for instance_id in instances {
        let instance_id_str = instance_id.as_str().to_owned();
        debug!(
            "Shutting down existing node instance {}, node={}:{}",
            instance_id_str, node_name, node_tag
        );

        ServiceMessenger::poll(
            &ctx.messenger,
            &ctx.bound_master_node,
            &ctx.master_instance_id,
            node_name,
            SHUTDOWN_SERVICE,
            Some(&ctx.bound_master_node),
            Some(&instance_id_str),
            Bytes::from_static(b"shutdown"),
            SHUTDOWN_TIMEOUT,
        )
        .await
        .map_err(|e| {
            format!(
                "Failed to shutdown node instance '{}': {}",
                instance_id_str, e
            )
        })?;

        match ctx
            .node_stack
            .remove_instance(node_name, node_tag, &instance_id)
        {
            Ok(true) => {}
            Ok(false) => {
                return Err(format!(
                    "Node instance '{}' not found in node stack",
                    instance_id_str
                ));
            }
            Err(e) => {
                return Err(format!(
                    "Failed to remove node instance '{}' from node stack: {}",
                    instance_id_str, e
                ));
            }
        }

        if let Ok(payload) =
            NodeAddFeedback::stdout(format!("{instance_id_str} has been stopped")).encode()
        {
            let _ = ctx.feedback_publisher.publish(payload).await;
        }
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
    let env_vars = match super::validate_goal_env_vars(&goal.env_vars) {
        Ok(vars) => vars,
        Err(e) => {
            return NodeAddResult::failure(&ctx.log_path, e.to_string());
        }
    };
    let _cleanup_guard = CleanupDir::new(cleanup_dir);

    let node_name = node_config.manifest.name.as_str().to_owned();
    let node_tag = node_config.manifest.tag.clone();

    let previous_snapshot_path = ctx
        .node_stack
        .find(&node_name, &node_tag)
        .map(|entity| entity.root_path().to_path_buf());

    if verify_codegen_fingerprint {
        // Verify that the node config fingerprint matches the one in the generated folder.
        // Even if the `.peppy` content will be regenerated in the copy, we want to make sure the code that
        // the user has written for their node matches the current interface version.
        let config_path = source_path.join(NODE_CONFIG_FILE);
        if let Err(e) =
            config::fingerprint::verify_codegen_fingerprint(&config_path, PEPPYGEN_OUTPUT_PATH)
        {
            return NodeAddResult::failure(
                &ctx.log_path,
                format!("Codegen fingerprint verification failed: {}", e),
            );
        }
    }

    // Copy the node folder to the peppy storage directory.
    let copied_path = match copy_node_to_storage(&source_path, &node_name, &node_tag) {
        Ok(path) => path,
        Err(e) => {
            return NodeAddResult::failure(
                &ctx.log_path,
                format!("Failed to copy node folder: {}", e),
            );
        }
    };

    // Validate that all dependency nodes exist in the stack and expose the required
    // interfaces before running add_cmd. This prevents confusing build failures when
    // peppygen is generated with incomplete interfaces due to missing dependencies.
    let dep_errors = validate_dependency_specs(&node_config, &node_name, &node_tag, |name, tag| {
        ctx.node_stack.find(name, tag).map(|e| e.config().clone())
    });
    if let Some(err) = dep_errors.into_iter().next() {
        std::fs::remove_dir_all(&copied_path).ok();
        return NodeAddResult::failure(
            &ctx.log_path,
            format!("Failed to add node config: {}", err),
        );
    }

    // Generate the peppygen library for the copied node
    let language = node_config.manifest.language;
    let subscribed_interfaces = collect_subscribed_interfaces(&node_config, &ctx.node_stack);
    if let Err(e) = generate_peppygen_for_node(
        language,
        &copied_path,
        subscribed_interfaces,
        &goal.git_hash,
    ) {
        // Clean up the copied folder on failure
        std::fs::remove_dir_all(&copied_path).ok();
        return NodeAddResult::failure(
            &ctx.log_path,
            format!("Failed to generate peppygen library: {}", e),
        );
    }

    // Run add_cmd on the copied folder with streaming output
    if let Err(e) = run_add_cmd_with_streaming(
        node_config.manifest.add_cmd.as_ref(),
        &copied_path,
        &env_vars,
        &ctx.feedback_publisher,
        Arc::clone(&ctx.log_file),
    )
    .await
    {
        // Clean up the copied folder on failure
        std::fs::remove_dir_all(&copied_path).ok();
        return NodeAddResult::failure(&ctx.log_path, format!("add_cmd failed: {}", e));
    }

    if let Err(e) = shutdown_existing_instances(&node_name, &node_tag, &ctx).await {
        std::fs::remove_dir_all(&copied_path).ok();
        return NodeAddResult::failure(
            &ctx.log_path,
            format!("Failed to shutdown existing node instances: {}", e),
        );
    }

    // Add the node config to the stack
    if let Err(e) = ctx.node_stack.push_config(node_config, false, &copied_path) {
        // Clean up the copied folder on failure
        std::fs::remove_dir_all(&copied_path).ok();
        return NodeAddResult::failure(&ctx.log_path, format!("Failed to add node config: {}", e));
    }

    if let Some(previous_snapshot_path) = previous_snapshot_path
        && previous_snapshot_path != copied_path
        && is_node_snapshot_path(&previous_snapshot_path, &node_name, &node_tag)
    {
        std::fs::remove_dir_all(&previous_snapshot_path).ok();
    }

    debug!(
        "Added node {}:{} at {}",
        node_name,
        node_tag,
        copied_path.display()
    );

    NodeAddResult::success(copied_path, &ctx.log_path)
}

async fn handle_cancel_request(
    _context: ServiceRequestContext,
    state: Arc<Mutex<NodeAddActionState>>,
) -> PeppyResult<Bytes> {
    // For now, we don't support cancellation of the add operation
    // Just acknowledge the request
    let state_guard = state.lock().await;
    if matches!(*state_guard, NodeAddActionState::Running { .. }) {
        Ok(Bytes::from_static(
            b"cancel acknowledged (operation cannot be interrupted)",
        ))
    } else {
        Ok(Bytes::from_static(
            b"cancel acknowledged (no operation in progress)",
        ))
    }
}

async fn handle_result_request(
    _context: ServiceRequestContext,
    state: Arc<Mutex<NodeAddActionState>>,
) -> PeppyResult<Bytes> {
    let mut state_guard = state.lock().await;

    match std::mem::replace(&mut *state_guard, NodeAddActionState::Idle) {
        NodeAddActionState::Running {
            started_at,
            timeout_secs,
        } => {
            // Still running, restore state and return pending status
            *state_guard = NodeAddActionState::Running {
                started_at,
                timeout_secs,
            };
            Ok(Bytes::from_static(
                b"result pending: operation still in progress",
            ))
        }
        NodeAddActionState::Completed { result } => {
            let payload =
                result
                    .encode()
                    .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                        identifier: "node_add_result".to_string(),
                        reason: format!("Failed to encode result: {}", e),
                    })?;
            *state_guard = NodeAddActionState::ResultSent { result };
            Ok(payload)
        }
        NodeAddActionState::ResultSent { result } => {
            // Result was already sent, restore state and return it again
            let payload =
                result
                    .encode()
                    .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                        identifier: "node_add_result".to_string(),
                        reason: format!("Failed to encode result: {}", e),
                    })?;
            *state_guard = NodeAddActionState::ResultSent { result };
            Ok(payload)
        }
        NodeAddActionState::Idle | NodeAddActionState::Rejected => {
            Ok(Bytes::from_static(b"result pending: no result available"))
        }
    }
}
