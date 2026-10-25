use crate::Result;
use crate::services::raw_calls::RawCallGate;
use crate::services::repo::cache::{UNOWNED_REPO_ID, repositories_list_path};
use crate::services::repo::refresh::read_or_create_repos;
use crate::services::repo::{
    attributable_repo_id, has_entry_with_identity, repo_entry_id, repo_source_to_json,
    source_identity, write_repo_entries,
};
use crate::services::response::{failure, into_service_answer};
use config::failure_kind::PeppyFailureKind;
use core_node_api::ServiceId;
use core_node_api::encoding::{RepoAddRequest, RepoAddResponse};
use core_node_api::names;
use daemon_config::consts::PeppyDirs;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::{Failure, ServiceAnswer, ServiceRequestContext};
use peppylib::{MessengerHandle, ServiceMessenger};
use serde_json::Value;
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_repo_add(
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
        ServiceId::RepoAdd.name(),
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(raw_calls.guard(ServiceId::RepoAdd.name(), move |context| {
                handle_repo_add_request(context, peppy_dirs.clone())
            }))
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

/// Adds the repository of the request to `repositories.json5`, or gives the
/// failure of the add: `rejected` for a request that does not decode, an
/// empty source or an id it cannot take, `precondition_failed` for a
/// repository or an id already in the list, and `attempt_failed` for a list
/// the daemon cannot read or write.
async fn handle_repo_add_request(
    context: ServiceRequestContext,
    peppy_dirs: PeppyDirs,
) -> ServiceAnswer {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload_bytes();

    let request = RepoAddRequest::decode(payload.as_ref())
        .map_err(|error| failure(PeppyFailureKind::Rejected, error))?;

    debug!(
        "Received `repo_add` request from {sender_instance_id}, source={:?}, id={:?}",
        request.source, request.id
    );

    let placement = RepoIdPlacement::from_request(&request)
        .map_err(|reason| failure(PeppyFailureKind::Rejected, reason))?;

    let identity = source_identity(&request.source);
    if identity.trim().is_empty() {
        return Err(failure(
            PeppyFailureKind::Rejected,
            "repository path/URL must not be empty",
        ));
    }

    let repos_path = repositories_list_path(&peppy_dirs);

    let _guard = crate::services::repo::repos_file_lock().lock();

    let mut repos = read_or_create_repos(&peppy_dirs)
        .map_err(|error| failure(PeppyFailureKind::AttemptFailed, error))?;

    let new_identity = identity.trim();
    if has_entry_with_identity(&repos, new_identity) {
        return Err(failure(
            PeppyFailureKind::PreconditionFailed,
            format!("repository '{}' already exists", new_identity),
        ));
    }

    let next_id = next_repo_id(&repos, placement)?;

    repos.push(repo_source_to_json(next_id, &request.source));
    write_repo_entries(&repos_path, &repos, "repositories").map_err(crate::Error::into_failure)?;

    drop(_guard);

    into_service_answer(RepoAddResponse::new().encode().map_err(Into::into))
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
/// the caller sends back: `precondition_failed` for an id another entry
/// holds or an id the list leaves no room for, `rejected` for a pinned id out
/// of range.
///
/// The id must be one a cache entry can be attributed to (below
/// [`UNOWNED_REPO_ID`]): `repo list` skips any other repository and its
/// nodes rank as unowned. Above `i64::MAX` the id would also make
/// `repositories.json5` unreadable, since serde_json5 reads integers as
/// `i64`, and every later repo, node and launch command would fail on it.
fn next_repo_id(repos: &[Value], placement: RepoIdPlacement) -> std::result::Result<u64, Failure> {
    let no_room = |reason: &str| failure(PeppyFailureKind::PreconditionFailed, reason);
    let mut ids = repos.iter().filter_map(repo_entry_id);
    let id = match placement {
        // The caller pinned this id (typically from the >= 2000 band
        // reserved for organization-added repositories) so it stays stable
        // across re-runs of whatever setup registered it. Claiming an id
        // another entry already holds would merge two repositories' entries
        // under one owner, so it is a refusal naming the id.
        RepoIdPlacement::Pinned(id) => {
            if ids.any(|existing| existing == id) {
                return Err(failure(
                    PeppyFailureKind::PreconditionFailed,
                    format!(
                        "repository id {id} is already in use; pick another (ids >= 2000 are \
                         reserved for manually pinned repositories)"
                    ),
                ));
            }
            id
        }
        RepoIdPlacement::BeforeAll => match ids.min() {
            None => DEFAULT_FIRST_REPO_ID,
            Some(min) => min.checked_sub(1).ok_or_else(|| {
                no_room(
                    "cannot add repo with top priority: existing minimum id is 0 (would underflow)",
                )
            })?,
        },
        RepoIdPlacement::AfterAll => match ids.max() {
            None => DEFAULT_FIRST_REPO_ID,
            Some(max) => max.checked_add(1).ok_or_else(|| {
                no_room("cannot add repo: existing maximum id is u64::MAX (would overflow)")
            })?,
        },
    };
    if attributable_repo_id(id).is_none() {
        return Err(match placement {
            RepoIdPlacement::Pinned(_) => failure(
                PeppyFailureKind::Rejected,
                format!("repository id {id} is out of range; pick an id below {UNOWNED_REPO_ID}"),
            ),
            RepoIdPlacement::BeforeAll | RepoIdPlacement::AfterAll => no_room(&format!(
                "cannot add repo: the assigned id {id} is out of range; repository ids must be \
                 below {UNOWNED_REPO_ID}"
            )),
        });
    }
    Ok(id)
}
