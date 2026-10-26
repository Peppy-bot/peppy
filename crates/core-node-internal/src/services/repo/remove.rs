use crate::Result;
use crate::services::raw_calls::RawCallGate;
use crate::services::repo::cache::repositories_list_path;
use crate::services::repo::refresh::{read_or_create_repos, reindex_after_change};
use crate::services::repo::{repo_entry_id, write_repo_entries};
use crate::services::response::{failure, into_service_answer};
use config::failure_kind::PeppyFailureKind;
use core_node_api::ServiceId;
use core_node_api::encoding::{RepoRemoveRequest, RepoRemoveResponse};
use core_node_api::names;
use daemon_config::consts::PeppyDirs;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::{Failure, ServiceAnswer, ServiceRequestContext};
use peppylib::{MessengerHandle, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

pub async fn listen_for_repo_remove(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    peppy_dirs: PeppyDirs,
    raw_calls: RawCallGate,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        core_node_name,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        ServiceId::RepoRemove.name(),
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(
                raw_calls.guard(ServiceId::RepoRemove.name(), move |context| {
                    handle_repo_remove_request(context, peppy_dirs.clone())
                }),
            )
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_repo_remove_request(
    context: ServiceRequestContext,
    peppy_dirs: PeppyDirs,
) -> ServiceAnswer {
    remove_repository(&context, &peppy_dirs)?;

    // The removal itself already landed; the report says whether the
    // re-read that makes it take effect worked.
    let response = match reindex_after_change(&peppy_dirs).await {
        Some(report) => {
            warn!("Re-indexing after the removal reported problems: {report}");
            RepoRemoveResponse::with_refresh_report(report)
        }
        None => RepoRemoveResponse::refreshed(),
    };
    into_service_answer(response.encode().map_err(Into::into))
}

/// Removes the repository the request names from `repositories.json5`, or
/// gives the failure of the removal: `rejected` for a request that does not
/// decode or an id the list does not hold, and `attempt_failed` for a list
/// the daemon cannot read or write.
fn remove_repository(
    context: &ServiceRequestContext,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<(), Failure> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload_bytes();

    let request = RepoRemoveRequest::decode(payload.as_ref())
        .map_err(|error| failure(PeppyFailureKind::Rejected, error))?;

    debug!(
        "Received `repo_remove` request from {sender_instance_id}, id={}",
        request.id
    );

    let repos_path = repositories_list_path(peppy_dirs);

    let _guard = crate::services::repo::repos_file_lock().lock();

    let mut repos = read_or_create_repos(peppy_dirs)
        .map_err(|error| failure(PeppyFailureKind::AttemptFailed, error))?;

    let target_id = request.id;
    let position = repos
        .iter()
        .position(|entry| repo_entry_id(entry) == Some(target_id));

    let Some(pos) = position else {
        return Err(failure(
            PeppyFailureKind::Rejected,
            format!("repository with id {} not found", request.id),
        ));
    };

    repos.remove(pos);

    write_repo_entries(&repos_path, &repos, "repositories").map_err(crate::Error::into_failure)
}
