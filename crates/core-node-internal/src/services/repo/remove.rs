use crate::Result;
use crate::encoding::{RepoRemoveRequest, RepoRemoveResponse};
use crate::names;
use crate::services::repo::refresh::{process_refresh, read_or_create_repos, write_cache};
use config::consts::PeppyDirs;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
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
        node_name,
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
    let sender_instance_id = context.message().instance_id();
    let (payload, needs_refresh) = handle_repo_remove_request_inner(&context, &peppy_dirs)
        .map_err(|e| PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        })?;

    if needs_refresh {
        let dirs = peppy_dirs.clone();
        match tokio::task::spawn_blocking(move || {
            let refresh_result = process_refresh(&dirs, &mut |_| {});
            (refresh_result, dirs)
        })
        .await
        {
            Ok((Ok((discovered, _excluded)), dirs)) => {
                if let Err(e) = write_cache(&dirs, &discovered) {
                    warn!("Failed to write cache after repo removal: {}", e);
                }
            }
            Ok((Err(e), _dirs)) => {
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

    let repos_path = peppy_dirs.conf_dir().join("repositories.json5");

    let _guard = crate::services::repo::repos_file_lock().lock();

    let mut repos = match read_or_create_repos(peppy_dirs) {
        Ok(repos) => repos,
        Err(e) => return Ok((RepoRemoveResponse::failure(e.to_string()).encode()?, false)),
    };

    let target_id = request.id as u64;
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

    let content = serde_json::to_string_pretty(&repos)
        .map_err(|e| crate::Error::Encoding(format!("failed to serialize repositories: {e}")))?;
    std::fs::write(&repos_path, content)?;

    drop(_guard);

    let payload = RepoRemoveResponse::success().encode()?;
    Ok((payload, true))
}
