use crate::Result;
use crate::names;
use crate::services::repo::cache::nodes_repo_cache_path;
use crate::services::repo::exclude::ExclusionSet;
use crate::services::repo::refresh::{parse_repo_entry, read_or_create_repos, walk_directory};
use config::consts::PeppyDirs;
use core_node_api::encoding::{
    RepoListNodeEntry, RepoListRequest, RepoListResponse, RepoSource, RepoSourceKind,
};
use peppylib::messaging::Iface;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceWireReceiver};
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
    let mut endpoint = messenger
        .expose_service(&ServiceWireReceiver::new(
            core_node_name,
            instance_id,
            node_name,
            Iface::native(),
            names::REPO_LIST,
        )?)
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
        Err(e) => {
            return RepoListResponse::failure(e.to_string())
                .encode()
                .map_err(Into::into);
        }
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

        let identity = source.identity();

        if exclusions.is_excluded(&identity) {
            debug!("Excluding repository from list: {}", identity);
            continue;
        }

        let repo_id_u64 = entry.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        let repo_id = match u32::try_from(repo_id_u64) {
            Ok(id) => id,
            Err(_) => {
                warn!(
                    "Skipping repository entry with id {} (exceeds u32 wire-format limit)",
                    repo_id_u64
                );
                continue;
            }
        };

        match source {
            RepoSource::Fs(path) => {
                if !path.exists() {
                    debug!("Skipping non-existent FS repository: {}", path.display());
                    continue;
                }
                let repo_label = path.to_string_lossy().into_owned();
                let walked =
                    walk_directory(&path, RepoSourceKind::Fs, None, None, &exclusions.fs_paths);
                for node in walked.nodes {
                    let key = (node.node_name.clone(), node.node_tag.clone());
                    let duplicate = !global_seen.insert(key);
                    all_entries.push(RepoListNodeEntry {
                        node_name: node.node_name,
                        node_tag: node.node_tag,
                        source_type: node.source_type,
                        path: node.path,
                        duplicate,
                        repo_id,
                        repo_label: repo_label.clone(),
                    });
                }
            }
            RepoSource::Git { repo_url, repo_ref } => {
                for cached in &cached_nodes {
                    if cached.get("source_type").and_then(|v| v.as_str()) != Some("git") {
                        continue;
                    }
                    if cached.get("source_uri").and_then(|v| v.as_str()) != Some(&repo_url) {
                        continue;
                    }
                    let resolved_ref = cached
                        .get("resolved_ref")
                        .and_then(|v| v.as_str())
                        .unwrap_or("HEAD");
                    // When the repo entry pins a specific ref, only match cached
                    // nodes whose resolved_ref equals it. Otherwise match any ref.
                    if let Some(pinned) = repo_ref.as_deref()
                        && !pinned.is_empty()
                        && pinned != resolved_ref
                    {
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
                    let repo_label = format!("{repo_url} (ref: {resolved_ref})");
                    all_entries.push(RepoListNodeEntry {
                        node_name: name.to_string(),
                        node_tag: tag.to_string(),
                        source_type: RepoSourceKind::Git,
                        path: cached
                            .get("path")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        duplicate,
                        repo_id,
                        repo_label,
                    });
                }
            }
            RepoSource::Url(url) => {
                warn!("Skipping URL repository (not yet supported): {}", url);
            }
        }
    }

    RepoListResponse::success(all_entries)
        .encode()
        .map_err(Into::into)
}

/// Read cached node entries from nodes.json5 in the cache directory.
fn read_cached_nodes(peppy_dirs: &PeppyDirs) -> Vec<Value> {
    let cache_path = nodes_repo_cache_path(peppy_dirs);
    if !cache_path.exists() {
        return Vec::new();
    }
    match std::fs::read_to_string(&cache_path) {
        Ok(content) => serde_json5::from_str(&content).unwrap_or_else(|e| {
            warn!(
                "Failed to parse nodes cache (nodes.json5) at {}: {e}",
                cache_path.display()
            );
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}
