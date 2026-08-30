use crate::Result;
use crate::services::repo::cache::{NodeCacheEntry, load_repo_cache};
use crate::services::repo::status::{self, RepoStatus};
use crate::services::repo::{ListedRepo, RepoNodes, listed_repositories, nodes_by_repository};
use crate::services::response::into_service_response;
use core_node_api::ServiceId;
use core_node_api::encoding::{
    RepoListNodeEntry, RepoListRepoEntry, RepoListRepoFailure, RepoListRepoFailureKind,
    RepoListRequest, RepoListResponse,
};
use core_node_api::names;
use daemon_config::consts::PeppyDirs;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::warn;

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
    let payload = context.message().payload_bytes();
    let _request = RepoListRequest::decode(payload.as_ref())?;

    let repos = match listed_repositories(peppy_dirs) {
        Ok(repos) => repos,
        Err(e) => {
            return RepoListResponse::failure(e.to_string())
                .encode()
                .map_err(Into::into);
        }
    };

    let statuses = status::read(peppy_dirs);
    let cached: Vec<NodeCacheEntry> = load_repo_cache(peppy_dirs).unwrap_or_else(|e| {
        warn!("Failed to read the nodes cache: {e}");
        Vec::new()
    });

    let mut all_entries: Vec<RepoListNodeEntry> = Vec::new();
    let mut all_repos: Vec<RepoListRepoEntry> = Vec::new();
    for RepoNodes { repo, nodes } in nodes_by_repository(&repos, &cached) {
        let status = statuses.iter().find(|s| s.id == u64::from(repo.id));
        all_repos.push(repo_entry(repo, status));
        all_entries.extend(nodes.iter().map(|node| RepoListNodeEntry {
            node_name: node.entry.node_name.to_string(),
            node_tag: node.entry.node_tag.to_string(),
            source_type: node.entry.origin.kind(),
            path: node.entry.origin.path_str().to_owned(),
            duplicate: node.shadowed_by.is_some(),
            repo_id: repo.id,
            repo_label: repo.label.clone(),
        }));
    }

    RepoListResponse::success(all_entries, all_repos)
        .encode()
        .map_err(Into::into)
}

/// Projects one repository's recorded status onto the wire. A repository
/// with no status line has never been through a refresh that recorded
/// one, so it reads as never-read rather than as failed.
fn repo_entry(repo: &ListedRepo, status: Option<&RepoStatus>) -> RepoListRepoEntry {
    RepoListRepoEntry {
        id: repo.id,
        label: repo.label.clone(),
        source_type: repo.source.kind(),
        last_read_unix_secs: status.and_then(|s| s.last_read_unix_secs),
        retained: status.is_some_and(|s| s.is_retained()),
        failure: status
            .and_then(|s| s.last_failure.as_ref())
            .map(|f| wire_failure(&f.kind, &f.message)),
    }
}

/// Projects a recorded failure onto the wire's closed kind. The status
/// file keeps the kind as text so a value written by a future peppy is a
/// label rather than a parse error; the wire has two kinds only, so an
/// unrecognized one reads as unreachable with its raw label kept in the
/// detail. Dropping the failure instead would show a retained repository
/// with no reason at all.
fn wire_failure(kind: &str, message: &str) -> RepoListRepoFailure {
    match RepoListRepoFailureKind::parse(kind) {
        Some(kind) => RepoListRepoFailure {
            kind,
            detail: message.to_owned(),
        },
        None => RepoListRepoFailure {
            kind: RepoListRepoFailureKind::Unreachable,
            detail: format!("{kind}: {message}"),
        },
    }
}
