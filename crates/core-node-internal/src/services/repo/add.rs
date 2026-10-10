use crate::Result;
use crate::services::repo::cache::repositories_list_path;
use crate::services::repo::refresh::read_or_create_repos;
use crate::services::repo::{
    has_entry_with_identity, repo_entry_id, repo_source_to_json, source_identity,
    write_repo_entries,
};
use crate::services::response::into_service_response;
use core_node_api::ServiceId;
use core_node_api::encoding::{RepoAddRequest, RepoAddResponse};
use core_node_api::names;
use daemon_config::consts::PeppyDirs;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyResult, ServiceMessenger};
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
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        ServiceId::RepoAdd.name(),
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
    into_service_response(
        &context,
        handle_repo_add_request_inner(&context, &peppy_dirs),
    )
}

fn handle_repo_add_request_inner(
    context: &ServiceRequestContext,
    peppy_dirs: &PeppyDirs,
) -> Result<Payload> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload_bytes();

    let request = RepoAddRequest::decode(payload.as_ref())?;

    debug!(
        "Received `repo_add` request from {sender_instance_id}, source={:?}, id={:?}",
        request.source, request.id
    );

    let placement = match RepoIdPlacement::from_request(&request) {
        Ok(placement) => placement,
        Err(reason) => {
            return RepoAddResponse::failure(reason)
                .encode()
                .map_err(Into::into);
        }
    };

    let identity = source_identity(&request.source);
    if identity.trim().is_empty() {
        return RepoAddResponse::failure("repository path/URL must not be empty")
            .encode()
            .map_err(Into::into);
    }

    let repos_path = repositories_list_path(peppy_dirs);

    let _guard = crate::services::repo::repos_file_lock().lock();

    let mut repos = match read_or_create_repos(peppy_dirs) {
        Ok(repos) => repos,
        Err(e) => {
            return RepoAddResponse::failure(e.to_string())
                .encode()
                .map_err(Into::into);
        }
    };

    let new_identity = identity.trim();
    if has_entry_with_identity(&repos, new_identity) {
        return RepoAddResponse::failure(format!("repository '{}' already exists", new_identity))
            .encode()
            .map_err(Into::into);
    }

    let next_id = match next_repo_id(&repos, placement) {
        Ok(id) => id,
        Err(reason) => {
            return RepoAddResponse::failure(reason)
                .encode()
                .map_err(Into::into);
        }
    };

    repos.push(repo_source_to_json(next_id, &request.source));
    write_repo_entries(&repos_path, &repos, "repositories")?;

    drop(_guard);

    RepoAddResponse::success().encode().map_err(Into::into)
}

/// The id an auto-assigned entry takes when the list holds no id yet.
const DEFAULT_FIRST_REPO_ID: u64 = 1000;

/// Which id a new `repositories.json5` entry takes, parsed from the `id`
/// and `top` fields of a [`RepoAddRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepoIdPlacement {
    /// The caller pinned this id.
    Pinned(u64),
    /// One below the lowest existing id, so the repository takes priority
    /// over every other one.
    BeforeAll,
    /// One above the highest existing id.
    AfterAll,
}

impl RepoIdPlacement {
    /// Refuses a request that both pins an id and asks for top priority:
    /// the two answer the same question (which id the new entry takes) in
    /// incompatible ways, and honouring one would silently ignore the other.
    fn from_request(request: &RepoAddRequest) -> std::result::Result<Self, &'static str> {
        match (request.id, request.top) {
            (Some(_), true) => Err(
                "cannot combine an explicit id with top priority: --id pins the id, --top derives one",
            ),
            (Some(id), false) => Ok(Self::Pinned(id)),
            (None, true) => Ok(Self::BeforeAll),
            (None, false) => Ok(Self::AfterAll),
        }
    }
}

/// The id the new entry takes in `repos` at `placement`, or the refusal
/// the caller sends back.
fn next_repo_id(repos: &[Value], placement: RepoIdPlacement) -> std::result::Result<u64, String> {
    let mut ids = repos.iter().filter_map(repo_entry_id);
    match placement {
        // The caller pinned this id (typically from the >= 2000 band
        // reserved for organization-added repositories) so it stays stable
        // across re-runs of whatever setup registered it. Claiming an id
        // another entry already holds would merge two repositories' entries
        // under one owner, so it is a refusal naming the id.
        RepoIdPlacement::Pinned(id) => {
            if ids.any(|existing| existing == id) {
                return Err(format!(
                    "repository id {id} is already in use; pick another (ids >= 2000 are reserved \
                     for manually pinned repositories)"
                ));
            }
            Ok(id)
        }
        RepoIdPlacement::BeforeAll => match ids.min() {
            None => Ok(DEFAULT_FIRST_REPO_ID),
            Some(min) => min.checked_sub(1).ok_or_else(|| {
                "cannot add repo with top priority: existing minimum id is 0 (would underflow)"
                    .to_owned()
            }),
        },
        RepoIdPlacement::AfterAll => match ids.max() {
            None => Ok(DEFAULT_FIRST_REPO_ID),
            Some(max) => max.checked_add(1).ok_or_else(|| {
                "cannot add repo: existing maximum id is u64::MAX (would overflow)".to_owned()
            }),
        },
    }
}
