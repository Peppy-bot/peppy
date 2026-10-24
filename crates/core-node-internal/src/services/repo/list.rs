use crate::Result;
use crate::services::repo::cache::{NodeCacheEntry, load_repo_cache};
use crate::services::repo::status::{self, RepoStatus};
use crate::services::repo::{ListedRepo, RepoNodes, listed_repositories, nodes_by_repository};
use crate::services::response::{failure, into_service_answer};
use config::failure_kind::PeppyFailureKind;
use core_node_api::ServiceId;
use core_node_api::encoding::{
    RepoListNodeEntry, RepoListRepoEntry, RepoListRepoFailedRead, RepoListRepoReadFailure,
    RepoListRequest, RepoListResponse,
};
use core_node_api::names;
use daemon_config::consts::PeppyDirs;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::{ServiceAnswer, ServiceRequestContext};
use peppylib::{MessengerHandle, ServiceMessenger};
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

/// Lists what will actually resolve.
///
/// Every source kind is read from `nodes.json5` rather than re-walked
/// live. Walking fs repositories on the fly would show whatever is on
/// disk right now, which is a different question: after a failed refresh
/// it would display the entries peppy refused, and at any time it would
/// display nodes added since the last refresh that cannot yet resolve.
/// Reading the cache is what makes a partial update legible.
///
/// A request that does not decode is `rejected`; a repository list the
/// daemon cannot read is `attempt_failed`.
async fn handle_repo_list_request(
    context: ServiceRequestContext,
    peppy_dirs: PeppyDirs,
) -> ServiceAnswer {
    let payload = context.message().payload_bytes();
    RepoListRequest::decode(payload.as_ref())
        .map_err(|error| failure(PeppyFailureKind::Rejected, error))?;

    let repos = listed_repositories(&peppy_dirs)
        .map_err(|error| failure(PeppyFailureKind::AttemptFailed, error))?;

    let statuses = status::read(&peppy_dirs);
    let cached: Vec<NodeCacheEntry> = load_repo_cache(&peppy_dirs).unwrap_or_else(|e| {
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

    into_service_answer(
        RepoListResponse::new(all_entries, all_repos)
            .encode()
            .map_err(Into::into),
    )
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
        failed_read: status
            .and_then(|s| s.last_failure.as_ref())
            .map(|f| wire_failed_read(&f.read_failure, &f.message)),
    }
}

/// Projects a recorded failed read onto the wire's closed read failure. The
/// status file keeps the read failure as text so a value written by a
/// future peppy is a label rather than a parse error; the wire has two read
/// failures only, so an unrecognized one reads as unreachable with its raw
/// label kept in the detail. Dropping the failed read instead would show a
/// retained repository with no reason at all.
fn wire_failed_read(read_failure: &str, message: &str) -> RepoListRepoFailedRead {
    match RepoListRepoReadFailure::parse(read_failure) {
        Some(read_failure) => RepoListRepoFailedRead {
            read_failure,
            detail: message.to_owned(),
        },
        None => RepoListRepoFailedRead {
            read_failure: RepoListRepoReadFailure::Unreachable,
            detail: format!("{read_failure}: {message}"),
        },
    }
}
