use crate::Result;
use crate::services::raw_calls::RawCallGate;
use crate::services::repo::refresh::reindex_after_change;
use crate::services::repo::{
    EXCLUDED_REPOS_FILE, has_entry_with_identity, normalize_repo_entries, parse_repo_entry,
    repo_entry_id, repo_source_to_json, source_identity, write_repo_entries,
};
use crate::services::response::{failure, into_service_answer};
use config::failure_kind::PeppyFailureKind;
use core_node_api::ServiceId;
use core_node_api::encoding::{RepoExcludeRequest, RepoExcludeResponse, RepoSourceKind};
use core_node_api::names;
use daemon_config::consts::PeppyDirs;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::{Failure, ServiceAnswer, ServiceRequestContext};
use peppylib::{MessengerHandle, ServiceMessenger};
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

pub async fn listen_for_repo_exclude(
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
        ServiceId::RepoExclude.name(),
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(
                raw_calls.guard(ServiceId::RepoExclude.name(), move |context| {
                    handle_repo_exclude_request(context, peppy_dirs.clone())
                }),
            )
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_repo_exclude_request(
    context: ServiceRequestContext,
    peppy_dirs: PeppyDirs,
) -> ServiceAnswer {
    exclude_repository(&context, &peppy_dirs)?;

    // The exclusion itself already landed; the report says whether the
    // re-read that makes it take effect worked.
    let response = match reindex_after_change(&peppy_dirs).await {
        Some(report) => {
            warn!("Re-indexing after the exclusion reported problems: {report}");
            RepoExcludeResponse::with_refresh_report(report)
        }
        None => RepoExcludeResponse::refreshed(),
    };
    into_service_answer(response.encode().map_err(Into::into))
}

/// Reads excluded repositories from `conf/excluded_repositories.json5`.
/// Returns an empty list if the file does not exist (no default seeding).
pub(crate) fn read_excluded_repos(peppy_dirs: &PeppyDirs) -> Result<Vec<Value>> {
    let conf_dir = peppy_dirs.conf_dir();
    std::fs::create_dir_all(&conf_dir)?;
    let repos_path = conf_dir.join(EXCLUDED_REPOS_FILE);

    let mut repos: Vec<Value> = if repos_path.exists() {
        let content = std::fs::read_to_string(&repos_path)?;
        serde_json5::from_str(&content).map_err(|e| {
            core_node_api::Error::Decoding(format!(
                "failed to parse excluded_repositories.json5: {e}"
            ))
        })?
    } else {
        Vec::new()
    };

    normalize_repo_entries(&mut repos, &repos_path, "excluded repositories")?;

    Ok(repos)
}

/// Parsed exclusion data used by both `repo refresh` and `repo list`.
pub(crate) struct ExclusionSet {
    /// Identities (path for fs, url for git/url) of excluded entries.
    pub(crate) identities: HashSet<String>,
    /// FS subtrees this machine does not answer for, applied to the
    /// locations a repository's index declares before any of them is read.
    pub(crate) fs_paths: Vec<PathBuf>,
    /// Structured list of all excluded entries for feedback reporting.
    pub(crate) entries: Vec<ExcludedEntry>,
}

pub(crate) struct ExcludedEntry {
    pub(crate) source_type: RepoSourceKind,
    pub(crate) identity: String,
}

impl ExclusionSet {
    /// Load exclusions from `excluded_repositories.json5`.
    /// Returns an empty set if the file is missing or unreadable.
    pub(crate) fn load(peppy_dirs: &PeppyDirs) -> Self {
        let raw = read_excluded_repos(peppy_dirs).unwrap_or_else(|e| {
            warn!("Failed to read excluded repositories: {e}");
            Vec::new()
        });

        let mut identities = HashSet::new();
        let mut fs_paths = Vec::new();
        let mut entries = Vec::new();

        for e in &raw {
            let Ok(source) = parse_repo_entry(e) else {
                continue;
            };
            let kind = source.kind();
            let identity = source_identity(&source);

            if kind == RepoSourceKind::Fs {
                fs_paths.push(PathBuf::from(&identity));
            }
            identities.insert(identity.clone());
            entries.push(ExcludedEntry {
                source_type: kind,
                identity,
            });
        }

        Self {
            identities,
            fs_paths,
            entries,
        }
    }

    pub(crate) fn is_excluded(&self, identity: &str) -> bool {
        self.identities.contains(identity)
    }
}

/// Adds the repository the request names to `excluded_repositories.json5`,
/// or gives the failure of the exclusion: `rejected` for a request that
/// does not decode or an empty source, `precondition_failed` for a
/// repository already excluded, and `attempt_failed` for a list the daemon
/// cannot read or write.
fn exclude_repository(
    context: &ServiceRequestContext,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<(), Failure> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload_bytes();

    let request = RepoExcludeRequest::decode(payload.as_ref())
        .map_err(|error| failure(PeppyFailureKind::Rejected, error))?;

    debug!(
        "Received `repo_exclude` request from {sender_instance_id}, source={:?}",
        request.source
    );

    let identity = source_identity(&request.source);
    if identity.trim().is_empty() {
        return Err(failure(
            PeppyFailureKind::Rejected,
            "repository path/URL must not be empty",
        ));
    }

    let repos_path = peppy_dirs.conf_dir().join(EXCLUDED_REPOS_FILE);

    let _guard = crate::services::repo::repos_file_lock().lock();

    let mut repos = read_excluded_repos(peppy_dirs)
        .map_err(|error| failure(PeppyFailureKind::AttemptFailed, error))?;

    let new_identity = identity.trim();
    if has_entry_with_identity(&repos, new_identity) {
        return Err(failure(
            PeppyFailureKind::PreconditionFailed,
            format!("repository '{}' already exists", new_identity),
        ));
    }

    let next_id = repos
        .iter()
        .filter_map(repo_entry_id)
        .max()
        .map(|max| max + 1)
        .unwrap_or(1);

    repos.push(repo_source_to_json(next_id, &request.source));
    repos.sort_by_key(|e| repo_entry_id(e).unwrap_or(0));
    write_repo_entries(&repos_path, &repos, "excluded repositories")
        .map_err(crate::Error::into_failure)
}
