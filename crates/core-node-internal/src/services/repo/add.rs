use crate::Result;
use crate::encoding::{RepoAddRequest, RepoAddResponse, RepoSource};
use crate::names;
use config::consts::PeppyDirs;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use serde_json::Value;
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
            .handle_requests(move |context| {
                handle_repo_add_request(context, peppy_dirs.clone())
            })
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

fn repo_source_to_json(source: &RepoSource) -> Value {
    match source {
        RepoSource::Git {
            repo_url,
            repo_ref,
        } => {
            let mut map = serde_json::Map::new();
            map.insert("type".to_string(), Value::String("git".to_string()));
            map.insert("url".to_string(), Value::String(repo_url.clone()));
            if let Some(r) = repo_ref {
                map.insert("ref".to_string(), Value::String(r.to_string()));
            }
            Value::Object(map)
        }
        RepoSource::Url(url) => {
            let mut map = serde_json::Map::new();
            map.insert("type".to_string(), Value::String("url".to_string()));
            map.insert("url".to_string(), Value::String(url.clone()));
            Value::Object(map)
        }
    }
}

fn repo_source_url(source: &RepoSource) -> &str {
    match source {
        RepoSource::Git { repo_url, .. } => repo_url,
        RepoSource::Url(url) => url,
    }
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

    let url = repo_source_url(&request.source);
    if url.trim().is_empty() {
        return RepoAddResponse::failure("repository URL must not be empty").encode();
    }

    // Ensure conf directory exists
    let conf_dir = peppy_dirs.conf_dir();
    if let Err(e) = std::fs::create_dir_all(&conf_dir) {
        return RepoAddResponse::failure(format!("failed to create conf directory: {e}")).encode();
    }

    let repos_path = conf_dir.join("repositories.json5");

    // Read existing repos or start fresh
    let mut repos: Vec<Value> = if repos_path.exists() {
        let content = std::fs::read_to_string(&repos_path)?;
        serde_json5::from_str(&content).map_err(|e| {
            crate::Error::Decoding(format!("failed to parse repositories.json5: {e}"))
        })?
    } else {
        Vec::new()
    };

    // Duplicate check — match on the URL field
    let new_url = url.trim();
    let is_duplicate = repos.iter().any(|entry| {
        entry
            .get("url")
            .and_then(|v| v.as_str())
            .is_some_and(|existing| existing == new_url)
    });

    if is_duplicate {
        return RepoAddResponse::failure(format!("repository '{new_url}' already exists"))
            .encode();
    }

    // Append and write back (JSON is valid JSON5, use pretty for user readability)
    repos.push(repo_source_to_json(&request.source));
    let content = serde_json::to_string_pretty(&repos)
        .map_err(|e| crate::Error::Encoding(format!("failed to serialize repositories: {e}")))?;
    std::fs::write(&repos_path, content)?;

    RepoAddResponse::success().encode()
}
