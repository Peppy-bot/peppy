use crate::Result;
use crate::services::repo::cache::repositories_list_path;
use crate::services::repo::refresh::{read_or_create_repos, reindex_after_change};
use crate::services::response::into_service_response;
use core_node_api::ServiceId;
use core_node_api::encoding::{RepoRemoveRequest, RepoRemoveResponse};
use core_node_api::names;
use daemon_config::consts::PeppyDirs;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

pub async fn listen_for_repo_remove(
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
        ServiceId::RepoRemove.name(),
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(move |context| handle_repo_remove_request(context, peppy_dirs.clone()))
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_repo_remove_request(
    context: ServiceRequestContext,
    peppy_dirs: PeppyDirs,
) -> PeppyResult<Payload> {
    let (mut response, needs_refresh) = into_service_response(
        &context,
        handle_repo_remove_request_inner(&context, &peppy_dirs),
    )?;

    // The removal itself already landed, so `success` stays true; the
    // report says whether the re-read that makes it take effect worked.
    if needs_refresh
        && let Some(report) = reindex_after_change(&peppy_dirs).await
    {
        warn!("Re-indexing after the removal reported problems: {report}");
        response = RepoRemoveResponse::success_with_refresh_report(report);
    }

    into_service_response(&context, response.encode().map_err(Into::into))
}

/// Returns `(response, needs_refresh)`. The response is returned
/// unencoded so the caller can fold the post-change re-index report into
/// it before putting it on the wire.
fn handle_repo_remove_request_inner(
    context: &ServiceRequestContext,
    peppy_dirs: &PeppyDirs,
) -> Result<(RepoRemoveResponse, bool)> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = RepoRemoveRequest::decode(payload.as_ref())?;

    debug!(
        "Received `repo_remove` request from {sender_instance_id}, id={}",
        request.id
    );

    let repos_path = repositories_list_path(peppy_dirs);

    let _guard = crate::services::repo::repos_file_lock().lock();

    let mut repos = match read_or_create_repos(peppy_dirs) {
        Ok(repos) => repos,
        Err(e) => {
            return Ok((RepoRemoveResponse::failure(e.to_string()), false));
        }
    };

    let target_id = request.id;
    let position = repos
        .iter()
        .position(|entry| entry.get("id").and_then(|v| v.as_u64()) == Some(target_id));

    let Some(pos) = position else {
        return Ok((
            RepoRemoveResponse::failure(format!("repository with id {} not found", request.id)),
            false,
        ));
    };

    repos.remove(pos);

    let content = json5_pretty::to_string_pretty(&repos).map_err(|e| {
        core_node_api::Error::Encoding(format!("failed to serialize repositories: {e}"))
    })?;
    std::fs::write(&repos_path, content)?;

    drop(_guard);

    Ok((RepoRemoveResponse::success(), true))
}
