use crate::Result;
use crate::encoding::{
    RepoRefreshFeedback, RepoRefreshGoal, RepoRefreshGoalResponse, RepoRefreshResult, RepoSource,
};
use crate::names;
use crate::services::action_loop::{ActionResult, ActionState, GoalHandler, run_action_loop};
use crate::services::node::checkout_repo_ref;
use crate::services::repo::exclude::ExclusionSet;
use crate::services::repo::normalize_repo_entries;
use config::consts::{NODE_CONFIG_FILE, PeppyDirs};
use config::node::NodeConfigParser;
use git2::build::RepoBuilder;
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
        self.encode()
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
            let mut current = state.lock().await;
            if matches!(*current, ActionState::Running { .. }) {
                let response = RepoRefreshGoalResponse::rejected(
                    "a repo refresh operation is already in progress",
                );
                *current = ActionState::Rejected;
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
            let dirs = peppy_dirs.clone();
            let result = match tokio::task::spawn_blocking(move || process_refresh(&dirs)).await {
                Ok(Ok((discovered, excluded))) => {
                    // Publish feedback for excluded repositories
                    for repo in &excluded {
                        let feedback =
                            RepoRefreshFeedback::new_excluded(&repo.source_type, &repo.identity);
                        if let Ok(payload) = feedback.encode() {
                            let _ = feedback_publisher.publish(payload).await;
                        }
                    }

                    // Publish feedback only for non-duplicate nodes
                    for node in discovered.iter().filter(|n| !n.duplicate) {
                        let feedback = RepoRefreshFeedback::new(
                            &node.node_name,
                            &node.node_tag,
                            &node.source_type,
                            &node.path,
                            node.variants.clone(),
                        );
                        if let Ok(payload) = feedback.encode() {
                            let _ = feedback_publisher.publish(payload).await;
                        }
                    }

                    // Write cache for all nodes (including duplicates, so
                    // `repo list` can display every source).
                    if let Err(e) = write_cache(&peppy_dirs, &discovered) {
                        warn!("Failed to write repo refresh cache: {}", e);
                    }

                    let unique_count = discovered.iter().filter(|n| !n.duplicate).count() as u32;
                    RepoRefreshResult::success(unique_count)
                }
                Ok(Err(e)) => RepoRefreshResult::failure(e.to_string()),
                Err(e) => RepoRefreshResult::failure(format!("task panicked: {}", e)),
            };

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

#[derive(Debug, Clone)]
pub(crate) struct DiscoveredNode {
    pub(crate) node_name: String,
    pub(crate) node_tag: String,
    pub(crate) source_type: String,
    pub(crate) path: String,
    pub(crate) source_uri: Option<String>,
    pub(crate) variants: Vec<String>,
    /// `true` when another repository (with lower id) already provides this
    /// `(name, tag)` pair. The node is still recorded so that `repo list` can
    /// display all sources.
    pub(crate) duplicate: bool,
}

/// A repository that was skipped during refresh because it appears in the
/// `excluded_repositories.json5` configuration.
#[derive(Debug, Clone)]
pub(crate) struct ExcludedRepo {
    pub(crate) source_type: String,
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
            crate::Error::Decoding(format!("failed to parse repositories.json5: {e}"))
        })?
    } else {
        let home = match dirs::home_dir() {
            Some(h) => h.to_string_lossy().into_owned(),
            None => {
                warn!(
                    "Could not determine home directory for default repositories; using current directory"
                );
                std::env::current_dir()
                    .map(|d| d.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| "/tmp".to_string())
            }
        };
        let content = DEFAULT_REPOS_TEMPLATE.replace("{home_dir}", &home);
        std::fs::write(&repos_path, &content)?;
        serde_json5::from_str(&content).map_err(|e| {
            crate::Error::Decoding(format!("failed to parse default repositories: {e}"))
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
) -> Result<(Vec<DiscoveredNode>, Vec<ExcludedRepo>)> {
    let repos = read_or_create_repos(peppy_dirs)?;

    let exclusions = ExclusionSet::load(peppy_dirs);

    let mut global_seen: HashSet<(String, String)> = HashSet::new();
    let mut all_nodes: Vec<DiscoveredNode> = Vec::new();
    let excluded_repos: Vec<ExcludedRepo> = exclusions
        .entries
        .iter()
        .map(|e| ExcludedRepo {
            source_type: e.source_type.clone(),
            identity: e.identity.clone(),
        })
        .collect();

    for entry in &repos {
        let Some(source) = parse_repo_entry(entry) else {
            warn!("Skipping unrecognized repository entry: {:?}", entry);
            continue;
        };

        let identity = source.identity();

        if exclusions.is_excluded(&identity) {
            debug!(
                "Excluding {} repository: {}",
                source.source_type(),
                identity
            );
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
                let mut repo_seen = HashSet::new();
                let mut repo_nodes = Vec::new();
                walk_directory(
                    &path,
                    "fs",
                    None,
                    &mut repo_seen,
                    &mut repo_nodes,
                    &exclusions.fs_paths,
                );
                for mut node in repo_nodes {
                    let key = (node.node_name.clone(), node.node_tag.clone());
                    if !global_seen.insert(key) {
                        node.duplicate = true;
                    }
                    all_nodes.push(node);
                }
            }
            RepoSource::Git { repo_url, repo_ref } => {
                match clone_and_walk_git_repo(&repo_url, repo_ref.as_deref(), peppy_dirs) {
                    Ok(nodes) => {
                        for mut node in nodes {
                            let key = (node.node_name.clone(), node.node_tag.clone());
                            if !global_seen.insert(key) {
                                node.duplicate = true;
                            }
                            all_nodes.push(node);
                        }
                    }
                    Err(e) => {
                        warn!("Failed to refresh git repository {}: {}", repo_url, e);
                    }
                }
            }
        }
    }

    Ok((all_nodes, excluded_repos))
}

/// Walk a directory looking for `peppy.json5` files, collecting discovered nodes.
///
/// Any directory whose path matches one of the `excluded_paths` entries is
/// pruned from the walk (neither descended into nor scanned for config files).
pub(crate) fn walk_directory(
    root: &Path,
    source_type: &str,
    source_uri: Option<&str>,
    seen: &mut HashSet<(String, String)>,
    nodes: &mut Vec<DiscoveredNode>,
    excluded_paths: &[PathBuf],
) {
    let excluded = excluded_paths.to_vec();
    let walker = ignore::WalkBuilder::new(root)
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
        if entry.file_name().to_string_lossy() != NODE_CONFIG_FILE {
            continue;
        }

        let config_path = entry.path();
        let parsed = match NodeConfigParser::from_path(config_path) {
            Ok(parsed) => parsed,
            Err(e) => {
                debug!(
                    "Skipping invalid peppy.json5 at {}: {}",
                    config_path.display(),
                    e
                );
                continue;
            }
        };

        let name = parsed.manifest_name().to_string();
        let tag = parsed.manifest_tag().to_string();
        let variants = parsed.variant_names();
        let key = (name.clone(), tag.clone());

        if !seen.insert(key) {
            continue;
        }

        let node_path = if source_type == "git" {
            // For git repos, store relative path from repo root
            config_path
                .parent()
                .and_then(|p| p.strip_prefix(root).ok())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            // For FS repos, store absolute path
            config_path
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        };

        nodes.push(DiscoveredNode {
            node_name: name,
            node_tag: tag,
            source_type: source_type.to_string(),
            path: node_path,
            source_uri: source_uri.map(|s| s.to_string()),
            variants,
            duplicate: false,
        });
    }
}

/// Shallow-clone a git repository and walk it for peppy.json5 files.
fn clone_and_walk_git_repo(
    repo_url: &str,
    repo_ref: Option<&str>,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<Vec<DiscoveredNode>, String> {
    let tmp_dir = peppy_dirs.tmp_dir();
    std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("failed to create tmp dir: {}", e))?;
    let tmp =
        tempfile::tempdir_in(&tmp_dir).map_err(|e| format!("failed to create temp dir: {}", e))?;

    let is_local = repo_url.starts_with('/') || repo_url.starts_with("file://");

    let mut builder = RepoBuilder::new();
    if !is_local {
        let mut fetch_opts = git2::FetchOptions::new();
        fetch_opts.depth(1);
        builder.fetch_options(fetch_opts);
    }

    let repo = builder
        .clone(repo_url, tmp.path())
        .map_err(|e| format!("failed to clone: {}", e))?;

    if let Some(r) = repo_ref {
        checkout_repo_ref(&repo, r)
            .map_err(|e| format!("failed to checkout ref '{}': {}", r, e))?;
    }

    let mut seen = HashSet::new();
    let mut nodes = Vec::new();
    walk_directory(
        tmp.path(),
        "git",
        Some(repo_url),
        &mut seen,
        &mut nodes,
        &[],
    );

    Ok(nodes)
}

/// Write cached node information for git/url repositories.
pub(crate) fn write_cache(peppy_dirs: &PeppyDirs, nodes: &[DiscoveredNode]) -> Result<()> {
    let cache_dir = peppy_dirs.cache_dir();
    std::fs::create_dir_all(&cache_dir)?;

    let cache_entries: Vec<Value> = nodes
        .iter()
        .map(|n| {
            let mut map = serde_json::Map::new();
            map.insert("node_name".to_string(), Value::String(n.node_name.clone()));
            map.insert("node_tag".to_string(), Value::String(n.node_tag.clone()));
            map.insert(
                "source_type".to_string(),
                Value::String(n.source_type.clone()),
            );
            if let Some(url) = &n.source_uri {
                map.insert("source_uri".to_string(), Value::String(url.clone()));
            }
            map.insert("path".to_string(), Value::String(n.path.clone()));
            if !n.variants.is_empty() {
                let variant_values: Vec<Value> = n
                    .variants
                    .iter()
                    .map(|v| Value::String(v.clone()))
                    .collect();
                map.insert("variants".to_string(), Value::Array(variant_values));
            }
            if n.duplicate {
                map.insert("duplicate".to_string(), Value::Bool(true));
            }
            Value::Object(map)
        })
        .collect();

    let content = serde_json::to_string_pretty(&cache_entries)
        .map_err(|e| crate::Error::Encoding(format!("failed to serialize cache: {e}")))?;
    std::fs::write(cache_dir.join("packages.json5"), content)?;
    Ok(())
}

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

        // Should contain 2 entries: home dir (fs) + nodes_hub (git)
        assert_eq!(repos.len(), 2, "default repos should have 2 entries");

        let fs_entry = &repos[0];
        assert_eq!(fs_entry.get("id").unwrap().as_u64().unwrap(), 1);
        assert_eq!(fs_entry.get("type").unwrap().as_str().unwrap(), "fs");
        let path_val = fs_entry.get("path").unwrap().as_str().unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(path_val, home.to_string_lossy().as_ref());

        let git_entry = &repos[1];
        assert_eq!(git_entry.get("id").unwrap().as_u64().unwrap(), 2);
        assert_eq!(git_entry.get("type").unwrap().as_str().unwrap(), "git");
        assert_eq!(
            git_entry.get("url").unwrap().as_str().unwrap(),
            "https://github.com/Peppy-bot/nodes_hub"
        );
        assert_eq!(git_entry.get("ref").unwrap().as_str().unwrap(), "main");
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
  schema_version: 1,
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

        let (discovered, excluded) = process_refresh(&peppy_dirs).unwrap();
        assert_eq!(discovered.len(), 1, "only non-excluded repo nodes returned");
        assert_eq!(discovered[0].node_name, "node_a");
        assert_eq!(excluded.len(), 1, "one repo should be excluded");
        assert_eq!(excluded[0].source_type, "fs");
        assert_eq!(excluded[0].identity, repo_b.display().to_string());
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

        let (discovered, excluded) = process_refresh(&peppy_dirs).unwrap();
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
        assert_eq!(excluded[0].source_type, "fs");
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

        let (discovered, excluded) = process_refresh(&peppy_dirs).unwrap();
        assert_eq!(discovered.len(), 1, "FS node should still be found");
        assert_eq!(discovered[0].node_name, "node_a");
        assert_eq!(excluded.len(), 1, "git repo should be excluded");
        assert_eq!(excluded[0].source_type, "git");
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
        let (discovered, excluded) = process_refresh(&peppy_dirs).unwrap();
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

        let (discovered, excluded) = process_refresh(&peppy_dirs).unwrap();
        assert_eq!(discovered.len(), 1, "FS node should still be found");
        assert_eq!(excluded.len(), 1, "url repo should be excluded");
        assert_eq!(excluded[0].source_type, "url");
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
}
