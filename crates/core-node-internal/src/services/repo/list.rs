use crate::Result;
use crate::encoding::{RepoListNodeEntry, RepoListRequest, RepoListResponse, RepoSource};
use crate::names;
use crate::services::repo::exclude::ExclusionSet;
use crate::services::repo::refresh::{parse_repo_entry, read_or_create_repos, walk_directory};
use config::consts::PeppyDirs;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use serde_json::Value;
use std::collections::HashSet;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

pub async fn listen_for_repo_list(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    peppy_dirs: PeppyDirs,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        core_node_name,
        instance_id,
        node_name,
        names::REPO_LIST,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(move |context| handle_repo_list_request(context, peppy_dirs.clone()))
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_repo_list_request(
    context: ServiceRequestContext,
    peppy_dirs: PeppyDirs,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id();
    handle_repo_list_request_inner(&context, &peppy_dirs).map_err(|e| {
        PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        }
    })
}

fn handle_repo_list_request_inner(
    context: &ServiceRequestContext,
    peppy_dirs: &PeppyDirs,
) -> Result<Payload> {
    let payload = context.message().payload();
    let _request = RepoListRequest::decode(payload.as_ref())?;

    let repos = match read_or_create_repos(peppy_dirs) {
        Ok(repos) => repos,
        Err(e) => return RepoListResponse::failure(e.to_string()).encode(),
    };

    let exclusions = ExclusionSet::load(peppy_dirs);

    // Read cached nodes for git/url repos
    let cached_nodes = read_cached_nodes(peppy_dirs);

    let mut global_seen: HashSet<(String, String)> = HashSet::new();
    let mut all_entries: Vec<RepoListNodeEntry> = Vec::new();

    for entry in &repos {
        let Some(source) = parse_repo_entry(entry) else {
            warn!("Skipping unrecognized repository entry: {:?}", entry);
            continue;
        };

        // Check if this repo is excluded by identity match.
        let identity = match &source {
            RepoSource::Fs(path) => path.to_string_lossy().into_owned(),
            RepoSource::Git { repo_url, .. } => repo_url.clone(),
            RepoSource::Url(url) => url.clone(),
        };

        if exclusions.is_excluded(&identity) {
            debug!("Excluding repository from list: {}", identity);
            continue;
        }

        match source {
            RepoSource::Fs(path) => {
                if !path.exists() {
                    debug!("Skipping non-existent FS repository: {}", path.display());
                    continue;
                }
                let mut repo_seen = HashSet::new();
                let mut discovered = Vec::new();
                walk_directory(
                    &path,
                    "fs",
                    None,
                    &mut repo_seen,
                    &mut discovered,
                    &exclusions.fs_paths,
                );
                for node in discovered {
                    let key = (node.node_name.clone(), node.node_tag.clone());
                    let duplicate = !global_seen.insert(key);
                    all_entries.push(RepoListNodeEntry {
                        node_name: node.node_name,
                        node_tag: node.node_tag,
                        source_type: node.source_type,
                        path: node.path,
                        variants: node.variants,
                        duplicate,
                    });
                }
            }
            RepoSource::Git { repo_url, .. } => {
                for cached in &cached_nodes {
                    if cached.get("source_type").and_then(|v| v.as_str()) != Some("git") {
                        continue;
                    }
                    if cached.get("source_uri").and_then(|v| v.as_str()) != Some(&repo_url) {
                        continue;
                    }
                    let name = cached
                        .get("node_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let tag = cached
                        .get("node_tag")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let key = (name.to_string(), tag.to_string());
                    let duplicate = !global_seen.insert(key);
                    let variants = cached
                        .get("variants")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_owned()))
                                .collect()
                        })
                        .unwrap_or_default();
                    all_entries.push(RepoListNodeEntry {
                        node_name: name.to_string(),
                        node_tag: tag.to_string(),
                        source_type: "git".to_string(),
                        path: cached
                            .get("path")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        variants,
                        duplicate,
                    });
                }
            }
            RepoSource::Url(url) => {
                warn!("Skipping URL repository (not yet supported): {}", url);
            }
        }
    }

    RepoListResponse::success(all_entries).encode()
}

/// Read cached node entries from packages.json5 in the cache directory.
fn read_cached_nodes(peppy_dirs: &PeppyDirs) -> Vec<Value> {
    let cache_path = peppy_dirs.cache_dir().join("packages.json5");
    if !cache_path.exists() {
        return Vec::new();
    }
    match std::fs::read_to_string(&cache_path) {
        Ok(content) => serde_json5::from_str(&content).unwrap_or_else(|e| {
            warn!(
                "Failed to parse packages cache at {}: {e}",
                cache_path.display()
            );
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}
