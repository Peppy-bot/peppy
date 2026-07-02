use crate::Result;
use crate::names;
use crate::services::repo::cache::repositories_list_path;
use crate::services::repo::refresh::{
    process_refresh, read_or_create_repos, write_cache, write_interface_cache, write_launcher_cache,
};
use crate::services::response::into_service_response;
use daemon_config::consts::PeppyDirs;
use core_node_api::encoding::{RepoRemoveRequest, RepoRemoveResponse};
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
        names::REPO_REMOVE,
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
    let (payload, needs_refresh) = into_service_response(
        &context,
        handle_repo_remove_request_inner(&context, &peppy_dirs),
    )?;

    if needs_refresh {
        let dirs = peppy_dirs.clone();
        match tokio::task::spawn_blocking(move || {
            let _guard = crate::services::repo::refresh_lock().lock();
            match process_refresh(&dirs, &mut |_| {}) {
                Ok(refreshed) => {
                    write_cache(&dirs, &refreshed.nodes)?;
                    write_launcher_cache(&dirs, &refreshed.launchers)?;
                    write_interface_cache(&dirs, &refreshed.interfaces)
                }
                Err(e) => Err(e),
            }
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                warn!("Failed to refresh after repo removal: {}", e);
            }
            Err(e) => {
                warn!("Refresh task panicked after repo removal: {}", e);
            }
        }
    }

    Ok(payload)
}

/// Returns `(payload, needs_refresh)`.
fn handle_repo_remove_request_inner(
    context: &ServiceRequestContext,
    peppy_dirs: &PeppyDirs,
) -> Result<(Payload, bool)> {
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
            return Ok((RepoRemoveResponse::failure(e.to_string()).encode()?, false));
        }
    };

    let target_id = request.id;
    let position = repos
        .iter()
        .position(|entry| entry.get("id").and_then(|v| v.as_u64()) == Some(target_id));

    let Some(pos) = position else {
        return Ok((
            RepoRemoveResponse::failure(format!("repository with id {} not found", request.id))
                .encode()?,
            false,
        ));
    };

    repos.remove(pos);

    let content = json5_pretty::to_string_pretty(&repos).map_err(|e| {
        core_node_api::Error::Encoding(format!("failed to serialize repositories: {e}"))
    })?;
    std::fs::write(&repos_path, content)?;

    drop(_guard);

    let payload = RepoRemoveResponse::success().encode()?;
    Ok((payload, true))
}
