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
    handle_repo_remove_request_inner(&context, &peppy_dirs).map_err(|e| {
        PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        }
    })
}

fn handle_repo_remove_request_inner(
    context: &ServiceRequestContext,
    peppy_dirs: &PeppyDirs,
) -> Result<Payload> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = RepoRemoveRequest::decode(payload.as_ref())?;

    debug!(
        "Received `repo_remove` request from {sender_instance_id}, id={}",
        request.id
    );

    let repos_path = peppy_dirs.conf_dir().join("repositories.json5");

    let mut repos = read_or_create_repos(peppy_dirs)?;

    let target_id = request.id as u64;
    let position = repos
        .iter()
        .position(|entry| entry.get("id").and_then(|v| v.as_u64()) == Some(target_id));

    let Some(pos) = position else {
        return RepoRemoveResponse::failure(format!("repository with id {} not found", request.id))
            .encode();
    };

    let is_fs = repos[pos].get("type").and_then(|v| v.as_str()) == Some("fs");

    repos.remove(pos);

    let content = serde_json::to_string_pretty(&repos)
        .map_err(|e| crate::Error::Encoding(format!("failed to serialize repositories: {e}")))?;
    std::fs::write(&repos_path, content)?;

    if !is_fs {
        match process_refresh(peppy_dirs) {
            Ok(discovered) => {
                if let Err(e) = write_cache(peppy_dirs, &discovered) {
                    warn!("Failed to write cache after repo removal: {}", e);
                }
            }
            Err(e) => {
                warn!("Failed to refresh after repo removal: {}", e);
            }
        }
    }

    RepoRemoveResponse::success().encode()
}
