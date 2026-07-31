use crate::Result;
use crate::services::repo::cache::{NodeCacheEntry, load_repo_cache};
use crate::services::repo::exclude::ExclusionSet;
use crate::services::repo::refresh::{parse_repo_entry, read_or_create_repos};
use crate::services::repo::status::{self, RepoStatus};
use crate::services::repo::{owning_repo_id, source_identity};
use crate::services::response::into_service_response;
use core_node_api::ServiceId;
use core_node_api::encoding::{
    RepoListNodeEntry, RepoListRepoEntry, RepoListRepoFailure, RepoListRequest, RepoListResponse,
};
use core_node_api::names;
use daemon_config::consts::PeppyDirs;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyResult, ServiceMessenger};
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
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        ServiceId::RepoList.name(),
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
    into_service_response(
        &context,
        handle_repo_list_request_inner(&context, &peppy_dirs),
    )
}

/// Lists what will actually resolve.
///
/// Every source kind is read from `nodes.json5` rather than re-walked
/// live. Walking fs repositories on the fly would show whatever is on
/// disk right now, which is a different question: after a failed refresh
/// it would display the entries peppy refused, and at any time it would
/// display nodes added since the last refresh that cannot yet resolve.
/// Reading the cache is what makes a partial update legible.
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
    let statuses = status::read(peppy_dirs);
    let cached: Vec<NodeCacheEntry> = load_repo_cache(peppy_dirs).unwrap_or_else(|e| {
        warn!("Failed to read the nodes cache: {e}");
        Vec::new()
    });

    // Identities claimed more than once by a single repository. Refresh
    // refuses such a repository, so this only fires for a cache written
    // by an older peppy or hand-edited, which is exactly when a user
    // needs to be told which entry is poisoned.
    let mut claims: HashSet<(u32, &str, &str)> = HashSet::new();
    let mut conflicted: HashSet<(u32, &str, &str)> = HashSet::new();
    for node in &cached {
        let key = (
            node.repo_id,
            node.node_name.as_str(),
            node.node_tag.as_str(),
        );
        if !claims.insert(key) {
            conflicted.insert(key);
        }
    }

    let mut global_seen: HashSet<(&str, &str)> = HashSet::new();
    let mut all_entries: Vec<RepoListNodeEntry> = Vec::new();
    let mut all_repos: Vec<RepoListRepoEntry> = Vec::new();

    for entry in &repos {
        let Some(source) = parse_repo_entry(entry) else {
            warn!("Skipping unrecognized repository entry: {:?}", entry);
            continue;
        };

        let identity = source_identity(&source);
        if exclusions.is_excluded(&identity) {
            debug!("Excluding repository from list: {}", identity);
            continue;
        }

        let repo_id_u64 = entry.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        let Ok(repo_id) = u32::try_from(repo_id_u64) else {
            warn!(
                "Skipping repository entry with id {} (exceeds u32 wire-format limit)",
                repo_id_u64
            );
            continue;
        };

        let repo_label = source.display_label();
        let status = statuses.iter().find(|s| s.id == repo_id_u64);
        all_repos.push(repo_entry(repo_id, &repo_label, &source, status));

        for node in &cached {
            if owning_repo_id(
                &repos,
                node.source_type,
                node.source_uri.as_deref(),
                node.resolved_ref.as_deref(),
                &node.path,
            ) != Some(repo_id_u64)
            {
                continue;
            }
            let key = (node.node_name.as_str(), node.node_tag.as_str());
            let duplicate = !global_seen.insert(key);
            all_entries.push(RepoListNodeEntry {
                node_name: node.node_name.clone(),
                node_tag: node.node_tag.clone(),
                source_type: node.source_type,
                path: node.path.clone(),
                duplicate,
                repo_id,
                repo_label: repo_label.clone(),
                conflict: conflicted.contains(&(repo_id, key.0, key.1)),
            });
        }
    }

    RepoListResponse::success(all_entries, all_repos)
        .encode()
        .map_err(Into::into)
}

/// Projects one repository's recorded status onto the wire. A repository
/// with no status line has never been through a refresh that recorded
/// one, so it reads as never-read rather than as failed.
fn repo_entry(
    repo_id: u32,
    label: &str,
    source: &core_node_api::encoding::RepoSource,
    status: Option<&RepoStatus>,
) -> RepoListRepoEntry {
    RepoListRepoEntry {
        id: repo_id,
        label: label.to_owned(),
        source_type: source.kind(),
        last_read_unix_secs: status.and_then(|s| s.last_read_unix_secs),
        retained: status.is_some_and(|s| s.is_retained()),
        failure: status
            .and_then(|s| s.last_failure.as_ref())
            .map(|f| RepoListRepoFailure {
                kind: f.kind.clone(),
                detail: f.message.clone(),
            }),
    }
}
