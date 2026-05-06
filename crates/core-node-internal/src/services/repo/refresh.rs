use crate::Result;
use crate::names;
use crate::services::action_loop::{ActionResult, ActionState, GoalHandler, run_action_loop};
use crate::services::node::clone_with_progress;
use crate::services::repo::cache::LauncherCacheEntry;
use crate::services::repo::exclude::ExclusionSet;
use crate::services::repo::normalize_repo_entries;
use config::consts::{NODE_CONFIG_FILE, PeppyDirs};
use config::launcher::{PeppyLauncherParser, PeppySchema};
use config::node::NodeConfigParser;
use core_node_api::encoding::{
    RepoRefreshFeedback, RepoRefreshGoal, RepoRefreshGoalResponse, RepoRefreshResult, RepoSource,
    RepoSourceKind,
};
use peppylib::messaging::{ServiceRequestContext, TopicPublisher};
use peppylib::types::Payload;
use peppylib::{ActionMessenger, MessengerHandle, PeppyError, PeppyResult};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// Directory names that should never be descended into while searching for
/// `peppy.json5` files.
pub(crate) const PRUNED_DIR_NAMES: &[&str] = &[
    ".git",
    ".peppy",
    "target",
    "node_modules",
    ".venv",
    "__pycache__",
];

pub async fn listen_for_repo_refresh(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    peppy_dirs: PeppyDirs,
) -> Result<JoinHandle<Result<()>>> {
    let action = ActionMessenger::expose(
        messenger,
        core_node_name,
        instance_id,
        node_name,
        names::REPO_REFRESH_ACTION,
    )
    .await?;

    let handler = RepoRefreshGoalHandler {
        peppy_dirs: peppy_dirs.clone(),
    };

    let handle = tokio::spawn(async move { run_action_loop(action, handler).await });

    Ok(handle)
}

impl ActionResult for RepoRefreshResult {
    fn identifier() -> &'static str {
        "repo_refresh_result"
    }

    fn encode_result(&self) -> crate::Result<Payload> {
        self.encode().map_err(Into::into)
    }
}

#[derive(Clone)]
struct RepoRefreshGoalHandler {
    peppy_dirs: PeppyDirs,
}

impl GoalHandler for RepoRefreshGoalHandler {
    type Result = RepoRefreshResult;

    async fn handle_goal(
        &self,
        context: ServiceRequestContext,
        feedback_publisher: TopicPublisher,
        state: Arc<Mutex<ActionState<RepoRefreshResult>>>,
    ) -> PeppyResult<Payload> {
        {
            let current = state.lock().await;
            if matches!(*current, ActionState::Running { .. }) {
                let response = RepoRefreshGoalResponse::rejected(
                    "a repo refresh operation is already in progress",
                );
                return response
                    .encode()
                    .map_err(|e| PeppyError::InvalidServiceRequest {
                        identifier: "repo_refresh".to_string(),
                        reason: e.to_string(),
                    });
            }
        }

        let payload = context.message().payload();
        if let Err(e) = RepoRefreshGoal::decode(payload.as_ref()) {
            let response =
                RepoRefreshGoalResponse::rejected(format!("invalid goal payload: {}", e));
            *state.lock().await = ActionState::Rejected;
            return response
                .encode()
                .map_err(|e| PeppyError::InvalidServiceRequest {
                    identifier: "repo_refresh".to_string(),
                    reason: e.to_string(),
                });
        }

        {
            let mut s = state.lock().await;
            *s = ActionState::Running {
                started_at: Instant::now(),
                timeout_secs: 300,
            };
        }

        let peppy_dirs = self.peppy_dirs.clone();
        let state_clone = Arc::clone(&state);

        tokio::spawn(async move {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RepoRefreshFeedback>();

            let drain = tokio::spawn(async move {
                while let Some(feedback) = rx.recv().await {
                    if let Ok(payload) = feedback.encode() {
                        let _ = feedback_publisher.publish(payload).await;
                    }
                }
            });

            let dirs = peppy_dirs;
            let scan = tokio::task::spawn_blocking(move || {
                let _guard = crate::services::repo::refresh_lock().lock();
                let mut emit = |fb: RepoRefreshFeedback| {
                    let _ = tx.send(fb);
                };
                match process_refresh(&dirs, &mut emit) {
                    Ok((discovered, launchers, excluded)) => {
                        // Write caches for all nodes/launchers (including
                        // duplicates, so `repo list` can display every
                        // source).
                        let unique_nodes =
                            discovered.iter().filter(|n| !n.duplicate).count() as u32;
                        let unique_launchers =
                            launchers.iter().filter(|l| !l.duplicate).count() as u32;
                        write_cache(&dirs, &discovered)?;
                        write_launcher_cache(&dirs, &launchers)?;
                        Ok((unique_nodes, unique_launchers, excluded))
                    }
                    Err(e) => Err(e),
                }
            })
            .await;

            let result = match scan {
                Ok(Ok((unique_nodes, unique_launchers, _excluded))) => {
                    RepoRefreshResult::success(unique_nodes, unique_launchers)
                }
                Ok(Err(e)) => {
                    warn!("Repo refresh failed: {}", e);
                    RepoRefreshResult::failure(e.to_string())
                }
                Err(e) => RepoRefreshResult::failure(format!("task panicked: {}", e)),
            };

            // Flush all pending feedbacks before marking the result ready —
            // the CLI stops draining feedback once it sees a concrete result.
            let _ = drain.await;

            let mut s = state_clone.lock().await;
            *s = ActionState::Completed { result };
        });

        let response = RepoRefreshGoalResponse::accepted();
        response
            .encode()
            .map_err(|e| PeppyError::InvalidServiceRequest {
                identifier: "repo_refresh".to_string(),
                reason: e.to_string(),
            })
    }
}

pub(crate) use crate::services::repo::cache::NodeCacheEntry;

/// A repository that was skipped during refresh because it appears in the
/// `excluded_repositories.json5` configuration.
#[derive(Debug, Clone)]
pub(crate) struct ExcludedRepo {
    pub(crate) source_type: RepoSourceKind,
    pub(crate) identity: String,
}

/// Parse a JSON entry from repositories.json5 into a `RepoSource`.
pub(crate) fn parse_repo_entry(entry: &Value) -> Option<RepoSource> {
    let typ = entry.get("type")?.as_str()?;
    match typ {
        "fs" => {
            let path = entry.get("path")?.as_str()?;
            Some(RepoSource::Fs(PathBuf::from(path)))
        }
        "git" => {
            let url = entry.get("url")?.as_str()?.to_owned();
            let repo_ref = entry
                .get("ref")
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned());
            Some(RepoSource::Git {
                repo_url: url,
                repo_ref,
            })
        }
        "url" => {
            let url = entry.get("url")?.as_str()?.to_owned();
            Some(RepoSource::Url(url))
        }
        _ => None,
    }
}

const DEFAULT_REPOS_TEMPLATE: &str = include_str!("../../../assets/default_repositories.json5");

/// Reads the repositories.json5 config file, creating it with defaults if it
/// does not exist yet.  Ensures every entry has an integer `id` field
/// (auto-assigns missing ids) and returns entries sorted by `id`.
pub(crate) fn read_or_create_repos(peppy_dirs: &PeppyDirs) -> Result<Vec<Value>> {
    let conf_dir = peppy_dirs.conf_dir();
    std::fs::create_dir_all(&conf_dir)?;
    let repos_path = conf_dir.join("repositories.json5");

    let mut repos: Vec<Value> = if repos_path.exists() {
        let content = std::fs::read_to_string(&repos_path)?;
        serde_json5::from_str(&content).map_err(|e| {
            core_node_api::Error::Decoding(format!("failed to parse repositories.json5: {e}"))
        })?
    } else {
        std::fs::write(&repos_path, DEFAULT_REPOS_TEMPLATE)?;
        serde_json5::from_str(DEFAULT_REPOS_TEMPLATE).map_err(|e| {
            core_node_api::Error::Decoding(format!("failed to parse default repositories: {e}"))
        })?
    };

    normalize_repo_entries(&mut repos, &repos_path, "repositories")?;

    Ok(repos)
}

/// Main synchronous processing: reads repos, walks each source, returns discovered nodes
/// and the list of repositories that were excluded.
///
/// Nodes whose `(name, tag)` pair was already seen in a higher-priority
/// repository (lower id) are kept in the result but marked as `duplicate`.
/// This allows the cache (and therefore `repo list`) to display all sources
/// while still counting unique nodes correctly.
pub(crate) fn process_refresh(
    peppy_dirs: &PeppyDirs,
    on_feedback: &mut dyn FnMut(RepoRefreshFeedback),
) -> Result<(
    Vec<NodeCacheEntry>,
    Vec<LauncherCacheEntry>,
    Vec<ExcludedRepo>,
)> {
    let (repos, exclusions) = {
        let _guard = crate::services::repo::repos_file_lock().lock();
        let repos = read_or_create_repos(peppy_dirs)?;
        let exclusions = ExclusionSet::load(peppy_dirs);
        (repos, exclusions)
    };

    let mut global_seen: HashSet<(String, String)> = HashSet::new();
    let mut global_seen_launchers: HashSet<String> = HashSet::new();
    let mut all_nodes: Vec<NodeCacheEntry> = Vec::new();
    let mut all_launchers: Vec<LauncherCacheEntry> = Vec::new();
    let excluded_repos: Vec<ExcludedRepo> = exclusions
        .entries
        .iter()
        .map(|e| ExcludedRepo {
            source_type: e.source_type,
            identity: e.identity.clone(),
        })
        .collect();

    for repo in &excluded_repos {
        on_feedback(RepoRefreshFeedback::new_excluded(
            repo.source_type,
            &repo.identity,
        ));
    }

    for entry in &repos {
        let Some(source) = parse_repo_entry(entry) else {
            warn!("Skipping unrecognized repository entry: {:?}", entry);
            continue;
        };

        let identity = source.identity();

        if exclusions.is_excluded(&identity) {
            debug!("Excluding {} repository: {}", source.kind(), identity);
            continue;
        }

        match source {
            RepoSource::Url(url) => {
                debug!("Skipping URL repository (not yet implemented): {}", url);
            }
            RepoSource::Fs(path) => {
                if !path.exists() {
                    debug!("Skipping non-existent FS repository: {}", path.display());
                    continue;
                }
                on_feedback(RepoRefreshFeedback::new_progress(format!(
                    "Scanning {}",
                    path.display()
                )));
                let mut repo_seen = HashSet::new();
                let mut repo_nodes = Vec::new();
                let mut repo_launchers_seen = HashSet::new();
                let mut repo_launchers = Vec::new();
                walk_directory(
                    &path,
                    RepoSourceKind::Fs,
                    None,
                    None,
                    &mut repo_seen,
                    &mut repo_nodes,
                    &mut repo_launchers_seen,
                    &mut repo_launchers,
                    &exclusions.fs_paths,
                );
                for mut node in repo_nodes {
                    let key = (node.node_name.clone(), node.node_tag.clone());
                    if !global_seen.insert(key) {
                        node.duplicate = true;
                    }
                    if !node.duplicate {
                        on_feedback(RepoRefreshFeedback::new(
                            &node.node_name,
                            &node.node_tag,
                            node.source_type,
                            &node.path,
                            node.variants.clone(),
                        ));
                    }
                    all_nodes.push(node);
                }
                for mut launcher in repo_launchers {
                    if !global_seen_launchers.insert(launcher.launcher_name.clone()) {
                        launcher.duplicate = true;
                    }
                    all_launchers.push(launcher);
                }
            }
            RepoSource::Git { repo_url, repo_ref } => {
                let ref_suffix = repo_ref
                    .as_deref()
                    .map(|r| format!(" (ref {})", r))
                    .unwrap_or_default();
                on_feedback(RepoRefreshFeedback::new_progress(format!(
                    "Cloning {}{}",
                    repo_url, ref_suffix
                )));
                match clone_and_walk_git_repo(
                    &repo_url,
                    repo_ref.as_deref(),
                    peppy_dirs,
                    on_feedback,
                ) {
                    Ok((nodes, launchers)) => {
                        for mut node in nodes {
                            let key = (node.node_name.clone(), node.node_tag.clone());
                            if !global_seen.insert(key) {
                                node.duplicate = true;
                            }
                            if !node.duplicate {
                                on_feedback(RepoRefreshFeedback::new(
                                    &node.node_name,
                                    &node.node_tag,
                                    node.source_type,
                                    &node.path,
                                    node.variants.clone(),
                                ));
                            }
                            all_nodes.push(node);
                        }
                        for mut launcher in launchers {
                            if !global_seen_launchers.insert(launcher.launcher_name.clone()) {
                                launcher.duplicate = true;
                            }
                            all_launchers.push(launcher);
                        }
                    }
                    Err(e) => {
                        warn!("Failed to refresh git repository {}: {}", repo_url, e);
                    }
                }
            }
        }
    }

    Ok((all_nodes, all_launchers, excluded_repos))
}

/// Walk a directory looking for `peppy.json5` (node) and any `.json5`
/// file whose body declares `peppy_schema: "launcher_v1"` (launcher),
/// collecting discovered nodes and launchers.
///
/// Any directory whose path matches one of the `excluded_paths` entries is
/// pruned from the walk (neither descended into nor scanned for config files).
#[allow(clippy::too_many_arguments)]
pub(crate) fn walk_directory(
    root: &Path,
    source_type: RepoSourceKind,
    source_uri: Option<&str>,
    resolved_ref: Option<&str>,
    nodes_seen: &mut HashSet<(String, String)>,
    nodes: &mut Vec<NodeCacheEntry>,
    launchers_seen: &mut HashSet<String>,
    launchers: &mut Vec<LauncherCacheEntry>,
    excluded_paths: &[PathBuf],
) {
    // Canonicalize the root so that paths emitted by the walker share a
    // common prefix representation with the excluded paths (which come from
    // `ExclusionSet::load` already canonicalized). Without this, macOS
    // `/var/...` symlinks break subdirectory exclusion: the walker emits
    // `/var/...` while excluded paths resolve to `/private/var/...`.
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let excluded = excluded_paths.to_vec();
    let walker = ignore::WalkBuilder::new(&root)
        .hidden(true)
        .filter_entry(move |entry| {
            if !entry.file_type().is_some_and(|ft| ft.is_dir()) {
                return true;
            }
            if entry.depth() == 0 {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            if PRUNED_DIR_NAMES.iter().any(|pruned| name == *pruned) {
                return false;
            }
            let entry_path = entry.path();
            !excluded.iter().any(|exc| entry_path.starts_with(exc))
        })
        .build();

    for entry in walker.flatten() {
        let file_name = entry.file_name().to_string_lossy();
        let config_path = entry.path();
        if file_name == NODE_CONFIG_FILE {
            collect_node_entry(
                &root,
                source_type,
                source_uri,
                resolved_ref,
                config_path,
                nodes_seen,
                nodes,
            );
        } else if has_json5_extension(config_path) {
            // Launchers have no fixed name — any `.json5` file whose
            // body declares `peppy_schema: "launcher_v1"` is treated as
            // a launcher. The `.json5` extension is the cheap pre-filter;
            // the schema check (inside `collect_launcher_entry`) is
            // authoritative.
            collect_launcher_entry(
                &root,
                source_type,
                source_uri,
                resolved_ref,
                config_path,
                launchers_seen,
                launchers,
            );
        }
    }
}

fn has_json5_extension(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "json5")
}

fn collect_node_entry(
    root: &Path,
    source_type: RepoSourceKind,
    source_uri: Option<&str>,
    resolved_ref: Option<&str>,
    config_path: &Path,
    seen: &mut HashSet<(String, String)>,
    nodes: &mut Vec<NodeCacheEntry>,
) {
    let parsed = match NodeConfigParser::from_path(config_path) {
        Ok(parsed) => parsed,
        Err(e) => {
            debug!(
                "Skipping invalid peppy.json5 at {}: {}",
                config_path.display(),
                e
            );
            return;
        }
    };

    let name = parsed.manifest_name().to_string();
    let tag = parsed.manifest_tag().to_string();
    let variants = parsed.variant_names();
    let key = (name.clone(), tag.clone());

    if !seen.insert(key) {
        return;
    }

    let node_path = relative_or_absolute_parent(root, config_path, source_type);

    nodes.push(NodeCacheEntry {
        node_name: name,
        node_tag: tag,
        source_type,
        path: node_path,
        source_uri: source_uri.map(|s| s.to_string()),
        variants,
        duplicate: false,
        resolved_ref: resolved_ref.map(|s| s.to_string()),
        checksum: None,
        repo_id: 0,
    });
}

fn collect_launcher_entry(
    root: &Path,
    source_type: RepoSourceKind,
    source_uri: Option<&str>,
    resolved_ref: Option<&str>,
    config_path: &Path,
    seen: &mut HashSet<String>,
    launchers: &mut Vec<LauncherCacheEntry>,
) {
    // Parse the file as a launcher; if it doesn't deserialize cleanly
    // or its `peppy_schema` isn't `LauncherV1`, it is just an unrelated
    // `.json5` file we should skip silently. Strict deserialization
    // (`#[serde(deny_unknown_fields)]` on `PeppyLauncher`) makes this a
    // robust signal — node configs etc. fail at the structural level.
    let parsed = match PeppyLauncherParser::from_path(config_path) {
        Ok(parsed) if parsed.peppy_schema == PeppySchema::LauncherV1 => parsed,
        Ok(_) => {
            // Some other peppy schema — not our concern.
            return;
        }
        Err(e) => {
            debug!(
                "Skipping non-launcher .json5 at {}: {}",
                config_path.display(),
                e
            );
            return;
        }
    };
    // We don't keep the parsed body — only its presence and schema
    // matter for cache discovery. Drop it explicitly to avoid retaining
    // the deployments vector longer than needed.
    drop(parsed);

    // Launcher name = basename without `.json5`. This matches
    // `resolve_launcher_path`, which appends `.json5` to a bare name
    // when looking up a launcher file.
    let Some(stem) = config_path.file_stem().and_then(|s| s.to_str()) else {
        return;
    };
    let name = stem.to_string();
    if name.is_empty() {
        return;
    }

    if !seen.insert(name.clone()) {
        return;
    }

    let launcher_path = relative_or_absolute_path(root, config_path, source_type);

    launchers.push(LauncherCacheEntry {
        launcher_name: name,
        source_type,
        source_uri: source_uri.map(|s| s.to_string()),
        resolved_ref: resolved_ref.map(|s| s.to_string()),
        path: launcher_path,
        duplicate: false,
        repo_id: 0,
    });
}

fn relative_or_absolute_parent(
    root: &Path,
    config_path: &Path,
    source_type: RepoSourceKind,
) -> String {
    if source_type == RepoSourceKind::Git {
        // For git repos, store the relative path from the repo root.
        config_path
            .parent()
            .and_then(|p| p.strip_prefix(root).ok())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        // For FS repos, store the absolute path.
        config_path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// Like `relative_or_absolute_parent` but returns the path of the file
/// itself (not its parent directory). Used for launchers, where the
/// cache should point at the `.json5` file rather than its containing
/// folder so callers can read it directly.
fn relative_or_absolute_path(
    root: &Path,
    config_path: &Path,
    source_type: RepoSourceKind,
) -> String {
    if source_type == RepoSourceKind::Git {
        config_path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        config_path.to_string_lossy().into_owned()
    }
}

/// Shallow-clone a git repository into `dst` and check out `repo_ref` if set,
/// forwarding throttled transfer-progress lines into `on_feedback`.
pub(crate) fn clone_shallow(
    repo_url: &str,
    repo_ref: Option<&str>,
    dst: &Path,
    on_feedback: &mut dyn FnMut(RepoRefreshFeedback),
) -> std::result::Result<git2::Repository, String> {
    clone_with_progress(repo_url, repo_ref, dst, true, &mut |line| {
        on_feedback(RepoRefreshFeedback::new_progress(line.to_owned()));
    })
}

/// Pick the ref string to persist in the cache so later batch installs can
/// re-fetch and re-check-out the same state.
///
/// `checkout_repo_ref` always detaches HEAD, which leaves `head().shorthand()`
/// equal to `"HEAD"` whenever the repo config pinned a ref — storing that
/// makes `add_batch` install the remote's default branch tip instead of the
/// pinned ref. Prefer the explicit config ref, then the cloned repo's
/// symbolic HEAD (for repos without a pin), and finally the commit OID.
fn resolve_ref_for_cache(repo: &git2::Repository, repo_ref: Option<&str>) -> String {
    if let Some(r) = repo_ref {
        let trimmed = r.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }

    if let Ok(head) = repo.head() {
        if let Some(short) = head.shorthand()
            && short != "HEAD"
        {
            return short.to_owned();
        }
        if let Some(oid) = head.target() {
            return oid.to_string();
        }
    }

    "HEAD".to_owned()
}

fn clone_and_walk_git_repo(
    repo_url: &str,
    repo_ref: Option<&str>,
    peppy_dirs: &PeppyDirs,
    on_feedback: &mut dyn FnMut(RepoRefreshFeedback),
) -> std::result::Result<(Vec<NodeCacheEntry>, Vec<LauncherCacheEntry>), String> {
    let tmp_dir = peppy_dirs.tmp_dir();
    std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("failed to create tmp dir: {}", e))?;
    let tmp =
        tempfile::tempdir_in(&tmp_dir).map_err(|e| format!("failed to create temp dir: {}", e))?;

    let repo = clone_shallow(repo_url, repo_ref, tmp.path(), on_feedback)?;
    let resolved_ref = resolve_ref_for_cache(&repo, repo_ref);

    let mut seen = HashSet::new();
    let mut nodes = Vec::new();
    let mut launchers_seen = HashSet::new();
    let mut launchers = Vec::new();
    walk_directory(
        tmp.path(),
        RepoSourceKind::Git,
        Some(repo_url),
        Some(&resolved_ref),
        &mut seen,
        &mut nodes,
        &mut launchers_seen,
        &mut launchers,
        &[],
    );

    Ok((nodes, launchers))
}

pub(crate) use crate::services::repo::cache::{write_cache, write_launcher_cache};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_or_create_repos_creates_file_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repos_path = peppy_dirs.conf_dir().join("repositories.json5");
        assert!(!repos_path.exists());

        let repos = read_or_create_repos(&peppy_dirs).unwrap();
        assert!(repos_path.exists(), "repositories.json5 should be created");

        // Don't pin the exact count — defaults grow over time. Just
        // assert that the well-known entries shipped in the template are
        // present with their expected shape.
        let by_id = |id: u64| -> &Value {
            repos
                .iter()
                .find(|r| r.get("id").and_then(|v| v.as_u64()) == Some(id))
                .unwrap_or_else(|| panic!("default repos should include id {id}"))
        };

        let nodes_hub = by_id(1000);
        assert_eq!(nodes_hub.get("type").unwrap().as_str().unwrap(), "git");
        assert_eq!(
            nodes_hub.get("url").unwrap().as_str().unwrap(),
            "https://github.com/Peppy-bot/nodes_hub"
        );
        assert_eq!(nodes_hub.get("ref").unwrap().as_str().unwrap(), "main");

        let launchers_hub = by_id(1001);
        assert_eq!(launchers_hub.get("type").unwrap().as_str().unwrap(), "git");
        assert_eq!(
            launchers_hub.get("url").unwrap().as_str().unwrap(),
            "https://github.com/Peppy-bot/launchers_hub.git"
        );
    }

    #[test]
    fn read_or_create_repos_reads_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join("repositories.json5"),
            r#"[{ "id": 1, "type": "fs", "path": "/custom/path" }]"#,
        )
        .unwrap();

        let repos = read_or_create_repos(&peppy_dirs).unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(
            repos[0].get("path").unwrap().as_str().unwrap(),
            "/custom/path"
        );
    }

    #[test]
    fn read_or_create_repos_subsequent_call_uses_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        // First call creates the file
        let _ = read_or_create_repos(&peppy_dirs).unwrap();

        // Overwrite with custom content
        let repos_path = peppy_dirs.conf_dir().join("repositories.json5");
        std::fs::write(
            &repos_path,
            r#"[{ "id": 1, "type": "fs", "path": "/other" }]"#,
        )
        .unwrap();

        // Second call should read the overwritten file, not re-create defaults
        let repos = read_or_create_repos(&peppy_dirs).unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].get("path").unwrap().as_str().unwrap(), "/other");
    }

    #[test]
    fn read_or_create_repos_rejects_duplicate_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join("repositories.json5"),
            r#"[
                { "id": 1, "type": "fs", "path": "/a" },
                { "id": 1, "type": "fs", "path": "/b" }
            ]"#,
        )
        .unwrap();

        let err = read_or_create_repos(&peppy_dirs).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("duplicate repository id 1"),
            "error should mention the duplicate id, got: {msg}"
        );
    }

    #[test]
    fn read_or_create_repos_auto_assigns_missing_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join("repositories.json5"),
            r#"[
                { "type": "fs", "path": "/no-id-a" },
                { "id": 5, "type": "fs", "path": "/has-id" },
                { "type": "fs", "path": "/no-id-b" }
            ]"#,
        )
        .unwrap();

        let repos = read_or_create_repos(&peppy_dirs).unwrap();
        assert_eq!(repos.len(), 3);

        // All entries should have ids and be sorted by id
        let ids: Vec<u64> = repos
            .iter()
            .map(|e| e.get("id").unwrap().as_u64().unwrap())
            .collect();
        assert_eq!(ids[0], 5, "explicit id 5 should be present");
        assert_eq!(ids[1], 6, "first missing id should be auto-assigned 6");
        assert_eq!(ids[2], 7, "second missing id should be auto-assigned 7");
    }

    /// Helper: write a minimal valid peppy.json5 into `dir`.
    fn write_peppy_json5(dir: &Path, name: &str, tag: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(NODE_CONFIG_FILE),
            format!(
                r#"{{
  peppy_schema: "node_v1",
  manifest: {{ name: "{name}", tag: "{tag}" }},
  interfaces: {{}},
  execution: {{ language: "rust", build_cmd: ["true"], run_cmd: ["true"] }},
}}"#
            ),
        )
        .unwrap();
    }

    /// Helper: write a repositories.json5 file.
    fn write_repos(peppy_dirs: &PeppyDirs, content: &str) {
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(conf_dir.join("repositories.json5"), content).unwrap();
    }

    /// Helper: write an excluded_repositories.json5 file.
    fn write_excluded_repos(peppy_dirs: &PeppyDirs, content: &str) {
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(conf_dir.join("excluded_repositories.json5"), content).unwrap();
    }

    #[test]
    fn process_refresh_skips_excluded_fs_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo_a = tmp.path().join("repo_a");
        let repo_b = tmp.path().join("repo_b");
        write_peppy_json5(&repo_a.join("node_a"), "node_a", "1.0.0");
        write_peppy_json5(&repo_b.join("node_b"), "node_b", "1.0.0");

        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}, {{ "id": 2, "type": "fs", "path": "{}" }}]"#,
                repo_a.display(),
                repo_b.display()
            ),
        );
        write_excluded_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
                repo_b.display()
            ),
        );

        let (discovered, _launchers, excluded) = process_refresh(&peppy_dirs, &mut |_| {}).unwrap();
        assert_eq!(discovered.len(), 1, "only non-excluded repo nodes returned");
        assert_eq!(discovered[0].node_name, "node_a");
        assert_eq!(excluded.len(), 1, "one repo should be excluded");
        assert_eq!(excluded[0].source_type, RepoSourceKind::Fs);
        // `identity` is canonicalized by `json_entry_identity`; the test
        // path is not, so compare against the canonical form to stay
        // robust on platforms with symlinked tempdirs (e.g. macOS's
        // `/var` → `/private/var`).
        assert_eq!(
            excluded[0].identity,
            std::fs::canonicalize(&repo_b)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        );
    }

    #[test]
    fn process_refresh_excludes_fs_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo = tmp.path().join("repo");
        write_peppy_json5(&repo.join("keep_node"), "keep_node", "1.0.0");
        write_peppy_json5(&repo.join("secret_node"), "secret_node", "1.0.0");

        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
                repo.display()
            ),
        );
        // Exclude the subdirectory, not the whole repo
        write_excluded_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
                repo.join("secret_node").display()
            ),
        );

        let (discovered, _launchers, excluded) = process_refresh(&peppy_dirs, &mut |_| {}).unwrap();
        assert_eq!(
            discovered.len(),
            1,
            "only the non-excluded subdirectory node should be found"
        );
        assert_eq!(discovered[0].node_name, "keep_node");
        // The subdirectory exclusion should still appear in the excluded list
        // so that it is reported as feedback to the user.
        assert_eq!(
            excluded.len(),
            1,
            "subdirectory exclusion should be reported"
        );
        assert_eq!(excluded[0].source_type, RepoSourceKind::Fs);
        assert!(
            excluded[0].identity.contains("secret_node"),
            "excluded identity should reference the subdirectory"
        );
    }

    #[test]
    fn process_refresh_skips_excluded_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo = tmp.path().join("repo");
        write_peppy_json5(&repo.join("node_a"), "node_a", "1.0.0");

        write_repos(
            &peppy_dirs,
            &format!(
                r#"[
                    {{ "id": 1, "type": "fs", "path": "{}" }},
                    {{ "id": 2, "type": "git", "url": "https://example.com/repo.git" }}
                ]"#,
                repo.display()
            ),
        );
        write_excluded_repos(
            &peppy_dirs,
            r#"[{ "id": 1, "type": "git", "url": "https://example.com/repo.git" }]"#,
        );

        let (discovered, _launchers, excluded) = process_refresh(&peppy_dirs, &mut |_| {}).unwrap();
        assert_eq!(discovered.len(), 1, "FS node should still be found");
        assert_eq!(discovered[0].node_name, "node_a");
        assert_eq!(excluded.len(), 1, "git repo should be excluded");
        assert_eq!(excluded[0].source_type, RepoSourceKind::Git);
        assert_eq!(excluded[0].identity, "https://example.com/repo.git");
    }

    #[test]
    fn process_refresh_no_exclusion_file() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo = tmp.path().join("repo");
        write_peppy_json5(&repo.join("node_a"), "node_a", "1.0.0");

        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
                repo.display()
            ),
        );

        // No excluded_repositories.json5 file
        let (discovered, _launchers, excluded) = process_refresh(&peppy_dirs, &mut |_| {}).unwrap();
        assert_eq!(discovered.len(), 1, "node should be found normally");
        assert!(excluded.is_empty(), "no repos should be excluded");
    }

    #[test]
    fn process_refresh_skips_excluded_url_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo = tmp.path().join("repo");
        write_peppy_json5(&repo.join("node_a"), "node_a", "1.0.0");

        write_repos(
            &peppy_dirs,
            &format!(
                r#"[
                    {{ "id": 1, "type": "fs", "path": "{}" }},
                    {{ "id": 2, "type": "url", "url": "https://example.com/packages" }}
                ]"#,
                repo.display()
            ),
        );
        write_excluded_repos(
            &peppy_dirs,
            r#"[{ "id": 1, "type": "url", "url": "https://example.com/packages" }]"#,
        );

        let (discovered, _launchers, excluded) = process_refresh(&peppy_dirs, &mut |_| {}).unwrap();
        assert_eq!(discovered.len(), 1, "FS node should still be found");
        assert_eq!(excluded.len(), 1, "url repo should be excluded");
        assert_eq!(excluded[0].source_type, RepoSourceKind::Url);
    }

    /// Helper: write a `.json5` launcher file at `path` (any name accepted).
    fn write_launcher_json5(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(
            path,
            r#"{
  peppy_schema: "launcher_v1",
  deployments: []
}"#,
        )
        .unwrap();
    }

    /// Process_refresh discovers launcher files (any `.json5` filename
    /// with `peppy_schema: "launcher_v1"`) alongside node files in the
    /// same FS walk, names them by basename, and dedupes across repos.
    #[test]
    fn process_refresh_discovers_launchers_from_fs_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo_a = tmp.path().join("repo_a");
        let repo_b = tmp.path().join("repo_b");
        // Three launchers with arbitrary names in repo_a; one collides
        // with a launcher named the same in repo_b. Note that none of
        // them use the legacy `peppy_launcher.json5` name.
        write_launcher_json5(&repo_a.join("openarm01_sim_teleop.json5"));
        write_launcher_json5(&repo_a.join("calibration.json5"));
        write_launcher_json5(&repo_a.join("nested").join("demo.json5"));
        write_launcher_json5(&repo_b.join("openarm01_sim_teleop.json5"));
        // Throw a node into repo_a too so we exercise the mixed walk.
        write_peppy_json5(&repo_a.join("node_a"), "node_a", "1.0.0");

        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}, {{ "id": 2, "type": "fs", "path": "{}" }}]"#,
                repo_a.display(),
                repo_b.display()
            ),
        );

        let (discovered, launchers, _excluded) = process_refresh(&peppy_dirs, &mut |_| {}).unwrap();
        assert_eq!(discovered.len(), 1, "node_a should be the only node");
        assert_eq!(
            launchers.len(),
            4,
            "every launcher kept (including duplicates)"
        );

        let unique: Vec<&LauncherCacheEntry> = launchers.iter().filter(|l| !l.duplicate).collect();
        let mut unique_names: Vec<&str> = unique.iter().map(|l| l.launcher_name.as_str()).collect();
        unique_names.sort_unstable();
        assert_eq!(
            unique_names,
            vec!["calibration", "demo", "openarm01_sim_teleop"]
        );

        // Path points at the `.json5` file, not its parent directory, so
        // downstream code can read it directly.
        let demo = unique
            .iter()
            .find(|l| l.launcher_name == "demo")
            .expect("demo launcher");
        assert!(
            demo.path.ends_with("demo.json5"),
            "launcher path should be the .json5 file itself: {}",
            demo.path
        );

        let dup: Vec<&LauncherCacheEntry> = launchers.iter().filter(|l| l.duplicate).collect();
        assert_eq!(dup.len(), 1);
        assert_eq!(dup[0].launcher_name, "openarm01_sim_teleop");
        assert!(
            dup[0].path.contains("repo_b"),
            "duplicate should be the lower-priority launcher: {}",
            dup[0].path
        );
    }

    /// `.json5` files that don't declare `peppy_schema: "launcher_v1"`
    /// must be skipped silently — they're unrelated configuration, not
    /// launchers.
    #[test]
    fn process_refresh_ignores_non_launcher_json5_files() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        // Real launcher.
        write_launcher_json5(&repo.join("real_launcher.json5"));
        // Random JSON5 file (not a launcher schema).
        std::fs::write(
            repo.join("settings.json5"),
            r#"{ theme: "dark", verbose: true }"#,
        )
        .unwrap();
        // Another node config in the wrong shape — this lives elsewhere
        // in the repo and must not be misclassified.
        std::fs::write(
            repo.join("manifest.json5"),
            r#"{ peppy_schema: "node_v1", manifest: { name: "x", tag: "1.0.0" }, interfaces: {}, execution: { language: "rust", build_cmd: ["true"], run_cmd: ["true"] } }"#,
        )
        .unwrap();

        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
                repo.display()
            ),
        );

        let (_discovered, launchers, _excluded) =
            process_refresh(&peppy_dirs, &mut |_| {}).unwrap();
        assert_eq!(
            launchers.len(),
            1,
            "only the real launcher should be cached"
        );
        assert_eq!(launchers[0].launcher_name, "real_launcher");
    }

    /// The launcher cache is written to disk by the refresh handler so
    /// downstream lookups can resolve launchers by name.
    #[test]
    fn process_refresh_writes_launcher_cache_via_write_launcher_cache() {
        use crate::services::repo::cache::launchers_repo_cache_path;

        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo = tmp.path().join("repo");
        write_launcher_json5(&repo.join("openarm01_sim_teleop.json5"));

        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
                repo.display()
            ),
        );

        let (_discovered, launchers, _excluded) =
            process_refresh(&peppy_dirs, &mut |_| {}).unwrap();
        write_launcher_cache(&peppy_dirs, &launchers).unwrap();

        let cache_path = launchers_repo_cache_path(&peppy_dirs);
        assert!(cache_path.exists(), "launcher cache should be written");

        let raw = std::fs::read_to_string(&cache_path).expect("read launcher cache");
        let parsed: serde_json::Value =
            serde_json::from_str(&raw).expect("launcher cache should be valid JSON");
        let arr = parsed.as_array().expect("expected JSON array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["launcher_name"], "openarm01_sim_teleop");
        assert_eq!(arr[0]["source_type"], "fs");
        let path_str = arr[0]["path"].as_str().expect("path should be a string");
        assert!(
            path_str.ends_with("openarm01_sim_teleop.json5"),
            "cached path should point at the .json5 file: {path_str}"
        );
    }

    /// End-to-end coverage of the launchers_hub flow: a git repository
    /// (cloned via libgit2's local transport) that ships a launcher at
    /// `openarm01/openarm01_teleop.json5` should land in `launcher.json5`
    /// with `launcher_name = "openarm01_teleop"`, the `file://` source
    /// URI, a non-`HEAD` resolved ref, and the relative path to the
    /// `.json5` file.
    #[test]
    fn process_refresh_discovers_launchers_from_git_repo() {
        use crate::services::repo::cache::launchers_repo_cache_path;

        // Build a real git repo with one launcher committed at the same
        // path layout as launchers_hub's openarm01_teleop launcher.
        let src_tmp = tempfile::tempdir().unwrap();
        let src = src_tmp.path();
        let repo = git2::Repository::init(src).expect("init repo");
        std::fs::create_dir_all(src.join("openarm01")).unwrap();
        std::fs::write(
            src.join("openarm01").join("openarm01_teleop.json5"),
            r#"{
                peppy_schema: "launcher_v1",
                deployments: []
            }"#,
        )
        .unwrap();
        let signature =
            git2::Signature::now("Peppy", "peppy@example.com").expect("create signature");
        let mut index = repo.index().expect("open index");
        index
            .add_path(Path::new("openarm01/openarm01_teleop.json5"))
            .expect("stage launcher");
        index.write().expect("write index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "add openarm01_teleop launcher",
            &tree,
            &[],
        )
        .expect("commit");
        let branch = repo
            .head()
            .expect("head")
            .shorthand()
            .expect("shorthand")
            .to_owned();

        // Configure peppy with a single git repo entry pointing at the
        // local source via `file://`.
        let peppy_tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(peppy_tmp.path());
        let repo_url = format!("file://{}", src.display());
        write_repos(
            &peppy_dirs,
            &format!(r#"[{{ "id": 1, "type": "git", "url": "{repo_url}", "ref": "{branch}" }}]"#,),
        );

        let (_nodes, launchers, _excluded) = process_refresh(&peppy_dirs, &mut |_| {}).unwrap();
        assert_eq!(launchers.len(), 1, "exactly one launcher expected");
        let launcher = &launchers[0];
        assert_eq!(launcher.launcher_name, "openarm01_teleop");
        assert_eq!(launcher.source_type, RepoSourceKind::Git);
        assert_eq!(launcher.source_uri.as_deref(), Some(repo_url.as_str()));
        assert_eq!(
            launcher.resolved_ref.as_deref(),
            Some(branch.as_str()),
            "resolved_ref should record the branch we cloned, not literal `HEAD`"
        );
        assert_eq!(launcher.path, "openarm01/openarm01_teleop.json5");
        assert!(!launcher.duplicate);

        // Round-trip through `write_launcher_cache` so we also lock in
        // the on-disk shape the user specified: `launcher_name` field
        // present, `entry_type` and `node_name` absent.
        write_launcher_cache(&peppy_dirs, &launchers).unwrap();
        let raw = std::fs::read_to_string(launchers_repo_cache_path(&peppy_dirs))
            .expect("read launcher cache");
        let parsed: serde_json::Value =
            serde_json::from_str(&raw).expect("launcher cache should be valid JSON");
        let entry = &parsed.as_array().expect("array")[0];
        assert_eq!(entry["launcher_name"], "openarm01_teleop");
        assert_eq!(entry["source_type"], "git");
        assert_eq!(entry["source_uri"], repo_url);
        assert_eq!(entry["resolved_ref"], branch);
        assert_eq!(entry["path"], "openarm01/openarm01_teleop.json5");
        assert!(
            entry.get("entry_type").is_none(),
            "entry_type should not be present in the on-disk schema"
        );
        assert!(
            entry.get("node_name").is_none(),
            "node_name should be replaced by launcher_name"
        );
    }

    #[test]
    fn process_refresh_emits_progress_feedback() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo = tmp.path().join("repo");
        write_peppy_json5(&repo.join("node_a"), "node_a", "1.0.0");

        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
                repo.display()
            ),
        );

        let mut feedbacks: Vec<RepoRefreshFeedback> = Vec::new();
        let _ = process_refresh(&peppy_dirs, &mut |fb| feedbacks.push(fb)).unwrap();

        let progress: Vec<&RepoRefreshFeedback> = feedbacks
            .iter()
            .filter(|f| !f.status_message.is_empty())
            .collect();
        assert!(
            progress
                .iter()
                .any(|f| f.status_message.starts_with("Scanning ")),
            "expected a 'Scanning …' progress feedback, got: {:?}",
            feedbacks
        );

        let discovered: Vec<&RepoRefreshFeedback> = feedbacks
            .iter()
            .filter(|f| !f.excluded && f.status_message.is_empty())
            .collect();
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].node_name, "node_a");
    }

    #[test]
    fn read_or_create_repos_sorts_by_id() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join("repositories.json5"),
            r#"[
                { "id": 3, "type": "fs", "path": "/third" },
                { "id": 1, "type": "fs", "path": "/first" },
                { "id": 2, "type": "fs", "path": "/second" }
            ]"#,
        )
        .unwrap();

        let repos = read_or_create_repos(&peppy_dirs).unwrap();
        assert_eq!(repos[0].get("path").unwrap().as_str().unwrap(), "/first");
        assert_eq!(repos[1].get("path").unwrap().as_str().unwrap(), "/second");
        assert_eq!(repos[2].get("path").unwrap().as_str().unwrap(), "/third");
    }

    /// Create a local git repo at `path` with `commit_count` commits on the
    /// current branch. Each commit adds a file `commit_N.txt`. Returns the
    /// OIDs of the created commits in order (oldest first).
    fn init_repo_with_commits(path: &Path, commit_count: usize) -> Vec<git2::Oid> {
        let repo = git2::Repository::init(path).expect("init repo");
        let signature =
            git2::Signature::now("Peppy", "peppy@example.com").expect("create signature");
        let mut commits: Vec<git2::Oid> = Vec::new();

        for i in 0..commit_count {
            let file_name = format!("commit_{i}.txt");
            std::fs::write(path.join(&file_name), format!("content {i}")).expect("write file");
            let mut index = repo.index().expect("open index");
            index.add_path(Path::new(&file_name)).expect("add to index");
            index.write().expect("write index");
            let tree_id = index.write_tree().expect("write tree");
            let tree = repo.find_tree(tree_id).expect("find tree");

            let parent_commits: Vec<git2::Commit> = commits
                .last()
                .map(|oid| vec![repo.find_commit(*oid).expect("find parent")])
                .unwrap_or_default();
            let parent_refs: Vec<&git2::Commit> = parent_commits.iter().collect();

            let oid = repo
                .commit(
                    Some("HEAD"),
                    &signature,
                    &signature,
                    &format!("commit {i}"),
                    &tree,
                    &parent_refs,
                )
                .expect("commit");
            commits.push(oid);
        }

        commits
    }

    fn count_commits(repo: &git2::Repository) -> usize {
        let mut revwalk = repo.revwalk().expect("revwalk");
        revwalk.push_head().expect("push head");
        revwalk.count()
    }

    /// libgit2's local transport (used for both raw paths and `file://`
    /// URLs) rejects `depth(1)` with "shallow fetch is not supported by the
    /// local transport". The clone helper has to detect those URLs and skip
    /// the shallow option, otherwise the clone fails outright. These two
    /// tests lock in that fallback behavior so a regression that re-enables
    /// `depth(1)` for local URLs would fail fast.
    #[test]
    fn clone_shallow_falls_back_to_full_clone_for_file_url() {
        let src_tmp = tempfile::tempdir().unwrap();
        init_repo_with_commits(src_tmp.path(), 3);

        let dst_tmp = tempfile::tempdir().unwrap();
        let dst = dst_tmp.path().join("clone");
        let repo_url = format!("file://{}", src_tmp.path().display());

        let repo = clone_shallow(&repo_url, None, &dst, &mut |_| {})
            .expect("file:// clone should succeed (depth must be skipped)");

        assert!(
            !dst.join(".git/shallow").exists(),
            "file:// goes through libgit2's local transport which rejects \
             depth(1); .git/shallow should not exist"
        );
        assert_eq!(count_commits(&repo), 3);
    }

    #[test]
    fn clone_shallow_falls_back_to_full_clone_for_raw_path() {
        let src_tmp = tempfile::tempdir().unwrap();
        init_repo_with_commits(src_tmp.path(), 3);

        let dst_tmp = tempfile::tempdir().unwrap();
        let dst = dst_tmp.path().join("clone");
        let repo_url = src_tmp.path().display().to_string();

        let repo = clone_shallow(&repo_url, None, &dst, &mut |_| {})
            .expect("raw-path clone should succeed (depth must be skipped)");

        assert!(!dst.join(".git/shallow").exists());
        assert_eq!(count_commits(&repo), 3);
    }

    #[test]
    fn clone_shallow_ref_checkout_works_on_local_clone() {
        let src_tmp = tempfile::tempdir().unwrap();
        let commits = init_repo_with_commits(src_tmp.path(), 3);
        let target_sha = commits[1].to_string();

        let dst_tmp = tempfile::tempdir().unwrap();
        let dst = dst_tmp.path().join("clone");
        let repo_url = format!("file://{}", src_tmp.path().display());

        let repo = clone_shallow(&repo_url, Some(&target_sha), &dst, &mut |_| {})
            .expect("clone_shallow with ref should succeed");

        let head = repo.head().expect("head");
        let head_oid = head.target().expect("head oid");
        assert_eq!(head_oid.to_string(), target_sha);
    }

    /// `checkout_repo_ref` detaches HEAD for every pinned ref, so
    /// `head().shorthand()` of the cloned repo is always `"HEAD"`. Guard
    /// against falling back to that literal — batch installs reuse
    /// `resolved_ref` as the fetch/checkout ref, so storing `"HEAD"`
    /// silently resolves to the remote's default branch instead of the
    /// pinned ref.
    #[test]
    fn resolve_ref_prefers_config_ref_over_detached_head() {
        let src_tmp = tempfile::tempdir().unwrap();
        let commits = init_repo_with_commits(src_tmp.path(), 2);
        let target_sha = commits[0].to_string();

        let dst_tmp = tempfile::tempdir().unwrap();
        let dst = dst_tmp.path().join("clone");
        let repo_url = format!("file://{}", src_tmp.path().display());

        let repo = clone_shallow(&repo_url, Some(&target_sha), &dst, &mut |_| {})
            .expect("clone_shallow with pinned commit should succeed");

        assert_eq!(
            repo.head().unwrap().shorthand(),
            Some("HEAD"),
            "precondition: checkout_repo_ref always detaches HEAD"
        );

        let resolved = resolve_ref_for_cache(&repo, Some(&target_sha));
        assert_eq!(
            resolved, target_sha,
            "pinned commit ref must be preserved so batch installs fetch it back"
        );
    }

    #[test]
    fn resolve_ref_trims_and_rejects_empty_config_ref() {
        let src_tmp = tempfile::tempdir().unwrap();
        init_repo_with_commits(src_tmp.path(), 1);

        let dst_tmp = tempfile::tempdir().unwrap();
        let dst = dst_tmp.path().join("clone");
        let repo_url = format!("file://{}", src_tmp.path().display());

        let repo = clone_shallow(&repo_url, None, &dst, &mut |_| {})
            .expect("clone_shallow without ref should succeed");

        let short = repo
            .head()
            .unwrap()
            .shorthand()
            .expect("default branch shorthand")
            .to_owned();
        assert_ne!(short, "HEAD", "fresh clone without ref must stay attached");

        assert_eq!(
            resolve_ref_for_cache(&repo, Some("  v1.0  ")),
            "v1.0",
            "config ref should be trimmed"
        );
        assert_eq!(
            resolve_ref_for_cache(&repo, Some("")),
            short,
            "empty config ref should fall through to the attached branch name"
        );
        assert_eq!(
            resolve_ref_for_cache(&repo, None),
            short,
            "absent config ref should fall through to the attached branch name"
        );
    }

    #[test]
    fn resolve_ref_falls_back_to_commit_oid_when_detached_without_config_ref() {
        let src_tmp = tempfile::tempdir().unwrap();
        let commits = init_repo_with_commits(src_tmp.path(), 1);

        let dst_tmp = tempfile::tempdir().unwrap();
        let dst = dst_tmp.path().join("clone");
        let repo_url = format!("file://{}", src_tmp.path().display());

        let repo = clone_shallow(&repo_url, None, &dst, &mut |_| {})
            .expect("clone_shallow should succeed");
        repo.set_head_detached(commits[0])
            .expect("detach head for test");
        assert_eq!(repo.head().unwrap().shorthand(), Some("HEAD"));

        assert_eq!(
            resolve_ref_for_cache(&repo, None),
            commits[0].to_string(),
            "detached HEAD with no config ref should record the commit OID"
        );
    }
}
