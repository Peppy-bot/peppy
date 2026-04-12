use crate::Result;
use crate::encoding::{
    RepoRefreshFeedback, RepoRefreshGoal, RepoRefreshGoalResponse, RepoRefreshResult, RepoSource,
};
use crate::names;
use crate::services::action_loop::{ActionResult, ActionState, GoalHandler, run_action_loop};
use crate::services::node::checkout_repo_ref;
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
const PRUNED_DIR_NAMES: &[&str] = &[
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
            let current = state.lock().await;
            if matches!(*current, ActionState::Running { .. }) {
                let response = RepoRefreshGoalResponse::rejected(
                    "a repo refresh operation is already in progress",
                );
                *state.lock().await = ActionState::Rejected;
                return response.encode().map_err(|e| PeppyError::InvalidServiceRequest {
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
            return response.encode().map_err(|e| PeppyError::InvalidServiceRequest {
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
            let result = match tokio::task::spawn_blocking(move || {
                process_refresh(&dirs)
            })
            .await
            {
                Ok(Ok(discovered)) => {
                    // Publish feedback for each discovered node
                    for node in &discovered {
                        let feedback = RepoRefreshFeedback::new(
                            &node.node_name,
                            &node.node_tag,
                            &node.source_type,
                            &node.path,
                        );
                        if let Ok(payload) = feedback.encode() {
                            let _ = feedback_publisher.publish(payload).await;
                        }
                    }

                    // Write cache for non-fs nodes
                    if let Err(e) = write_cache(&peppy_dirs, &discovered) {
                        warn!("Failed to write repo refresh cache: {}", e);
                    }

                    RepoRefreshResult::success(discovered.len() as u32)
                }
                Ok(Err(e)) => RepoRefreshResult::failure(e.to_string()),
                Err(e) => RepoRefreshResult::failure(format!("task panicked: {}", e)),
            };

            let mut s = state_clone.lock().await;
            *s = ActionState::Completed { result };
        });

        let response = RepoRefreshGoalResponse::accepted();
        response.encode().map_err(|e| PeppyError::InvalidServiceRequest {
            identifier: "repo_refresh".to_string(),
            reason: e.to_string(),
        })
    }
}

#[derive(Debug, Clone)]
struct DiscoveredNode {
    node_name: String,
    node_tag: String,
    source_type: String,
    path: String,
    source_url: Option<String>,
}

/// Parse a JSON entry from repositories.json5 into a `RepoSource`.
fn parse_repo_entry(entry: &Value) -> Option<RepoSource> {
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

/// Builds the default repository list (user home directory) when no config file exists.
fn default_repos() -> Vec<Value> {
    let mut repos = Vec::new();
    if let Some(home) = dirs::home_dir() {
        let mut map = serde_json::Map::new();
        map.insert("type".to_string(), Value::String("fs".to_string()));
        map.insert(
            "path".to_string(),
            Value::String(home.to_string_lossy().into_owned()),
        );
        repos.push(Value::Object(map));
    }
    repos
}

/// Main synchronous processing: reads repos, walks each source, returns discovered nodes.
fn process_refresh(peppy_dirs: &PeppyDirs) -> Result<Vec<DiscoveredNode>> {
    let repos_path = peppy_dirs.conf_dir().join("repositories.json5");

    let repos: Vec<Value> = if repos_path.exists() {
        let content = std::fs::read_to_string(&repos_path)?;
        serde_json5::from_str(&content).map_err(|e| {
            crate::Error::Decoding(format!("failed to parse repositories.json5: {e}"))
        })?
    } else {
        default_repos()
    };

    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut all_nodes: Vec<DiscoveredNode> = Vec::new();

    for entry in &repos {
        let Some(source) = parse_repo_entry(entry) else {
            warn!("Skipping unrecognized repository entry: {:?}", entry);
            continue;
        };

        match source {
            RepoSource::Url(url) => {
                debug!("Skipping URL repository (not yet implemented): {}", url);
            }
            RepoSource::Fs(path) => {
                if !path.exists() {
                    debug!("Skipping non-existent FS repository: {}", path.display());
                    continue;
                }
                walk_directory(&path, "fs", None, &mut seen, &mut all_nodes);
            }
            RepoSource::Git { repo_url, repo_ref } => {
                match clone_and_walk_git_repo(&repo_url, repo_ref.as_deref(), peppy_dirs) {
                    Ok(nodes) => {
                        for node in nodes {
                            let key = (node.node_name.clone(), node.node_tag.clone());
                            if seen.insert(key) {
                                all_nodes.push(node);
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to refresh git repository {}: {}", repo_url, e);
                    }
                }
            }
        }
    }

    Ok(all_nodes)
}

/// Walk a directory looking for `peppy.json5` files, collecting discovered nodes.
fn walk_directory(
    root: &Path,
    source_type: &str,
    source_url: Option<&str>,
    seen: &mut HashSet<(String, String)>,
    nodes: &mut Vec<DiscoveredNode>,
) {
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .filter_entry(|entry| {
            if !entry.file_type().is_some_and(|ft| ft.is_dir()) {
                return true;
            }
            if entry.depth() == 0 {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            !PRUNED_DIR_NAMES.iter().any(|pruned| name == *pruned)
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
            source_url: source_url.map(|s| s.to_string()),
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
    std::fs::create_dir_all(&tmp_dir)
        .map_err(|e| format!("failed to create tmp dir: {}", e))?;
    let tmp = tempfile::tempdir_in(&tmp_dir)
        .map_err(|e| format!("failed to create temp dir: {}", e))?;

    let mut fetch_opts = git2::FetchOptions::new();
    fetch_opts.depth(1);

    let repo = RepoBuilder::new()
        .fetch_options(fetch_opts)
        .clone(repo_url, tmp.path())
        .map_err(|e| format!("failed to shallow clone: {}", e))?;

    if let Some(r) = repo_ref {
        checkout_repo_ref(&repo, r)
            .map_err(|e| format!("failed to checkout ref '{}': {}", r, e))?;
    }

    let mut seen = HashSet::new();
    let mut nodes = Vec::new();
    walk_directory(tmp.path(), "git", Some(repo_url), &mut seen, &mut nodes);

    Ok(nodes)
}

/// Write cached node information for git/url repositories.
fn write_cache(peppy_dirs: &PeppyDirs, nodes: &[DiscoveredNode]) -> Result<()> {
    let cache_dir = peppy_dirs.cache_dir();
    std::fs::create_dir_all(&cache_dir)?;

    let cache_entries: Vec<Value> = nodes
        .iter()
        .filter(|n| n.source_type != "fs")
        .map(|n| {
            let mut map = serde_json::Map::new();
            map.insert(
                "node_name".to_string(),
                Value::String(n.node_name.clone()),
            );
            map.insert("node_tag".to_string(), Value::String(n.node_tag.clone()));
            map.insert(
                "source_type".to_string(),
                Value::String(n.source_type.clone()),
            );
            if let Some(url) = &n.source_url {
                map.insert("source_url".to_string(), Value::String(url.clone()));
            }
            map.insert("path".to_string(), Value::String(n.path.clone()));
            Value::Object(map)
        })
        .collect();

    let content = serde_json::to_string_pretty(&cache_entries)
        .map_err(|e| crate::Error::Encoding(format!("failed to serialize cache: {e}")))?;
    std::fs::write(cache_dir.join("repositories.json5"), content)?;
    Ok(())
}
