use crate::Result;
use crate::encoding::{RepoAddRequest, RepoAddResponse};
use crate::names;
use crate::services::repo::refresh::read_or_create_repos;
use crate::services::repo::{json_entry_identity, repo_source_to_json};
use config::consts::PeppyDirs;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_repo_add(
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
        names::REPO_ADD,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(move |context| handle_repo_add_request(context, peppy_dirs.clone()))
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_repo_add_request(
    context: ServiceRequestContext,
    peppy_dirs: PeppyDirs,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id();
    handle_repo_add_request_inner(&context, &peppy_dirs).map_err(|e| {
        PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        }
    })
}

fn handle_repo_add_request_inner(
    context: &ServiceRequestContext,
    peppy_dirs: &PeppyDirs,
) -> Result<Payload> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = RepoAddRequest::decode(payload.as_ref())?;

    debug!(
        "Received `repo_add` request from {sender_instance_id}, source={:?}",
        request.source
    );

    let identity = request.source.identity();
    if identity.trim().is_empty() {
        return RepoAddResponse::failure("repository path/URL must not be empty").encode();
    }

    let repos_path = peppy_dirs.conf_dir().join("repositories.json5");

    let _guard = crate::services::repo::repos_file_lock().lock();

    let mut repos = match read_or_create_repos(peppy_dirs) {
        Ok(repos) => repos,
        Err(e) => return RepoAddResponse::failure(e.to_string()).encode(),
    };

    let new_identity = identity.trim();
    let is_duplicate = repos
        .iter()
        .any(|entry| json_entry_identity(entry).is_some_and(|existing| existing == new_identity));

    if is_duplicate {
        return RepoAddResponse::failure(format!("repository '{}' already exists", new_identity))
            .encode();
    }

    let next_id = if request.top {
        match repos
            .iter()
            .filter_map(|e| e.get("id").and_then(|v| v.as_u64()))
            .min()
        {
            Some(min) => match min.checked_sub(1) {
                Some(n) => n,
                None => {
                    return RepoAddResponse::failure(
                        "cannot add repo with top priority: existing minimum id is 0 (would underflow)",
                    )
                    .encode();
                }
            },
            None => 1000,
        }
    } else {
        repos
            .iter()
            .filter_map(|e| e.get("id").and_then(|v| v.as_u64()))
            .max()
            .map(|max| max + 1)
            .unwrap_or(1000)
    };

    repos.push(repo_source_to_json(next_id, &request.source));
    let content = serde_json::to_string_pretty(&repos)
        .map_err(|e| crate::Error::Encoding(format!("failed to serialize repositories: {e}")))?;
    std::fs::write(&repos_path, content)?;

    drop(_guard);

    RepoAddResponse::success().encode()
}
