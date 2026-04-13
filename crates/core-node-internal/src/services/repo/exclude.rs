use crate::Result;
use crate::encoding::{RepoExcludeRequest, RepoExcludeResponse};
use crate::names;
use crate::services::repo::{json_entry_identity, normalize_repo_entries, repo_source_to_json};
use config::consts::PeppyDirs;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
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
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        core_node_name,
        instance_id,
        node_name,
        names::REPO_EXCLUDE,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(move |context| {
                handle_repo_exclude_request(context, peppy_dirs.clone())
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_repo_exclude_request(
    context: ServiceRequestContext,
    peppy_dirs: PeppyDirs,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id();
    handle_repo_exclude_request_inner(&context, &peppy_dirs).map_err(|e| {
        PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        }
    })
}

/// Reads excluded repositories from `conf/excluded_repositories.json5`.
/// Returns an empty list if the file does not exist (no default seeding).
pub(crate) fn read_excluded_repos(peppy_dirs: &PeppyDirs) -> Result<Vec<Value>> {
    let conf_dir = peppy_dirs.conf_dir();
    std::fs::create_dir_all(&conf_dir)?;
    let repos_path = conf_dir.join("excluded_repositories.json5");

    let mut repos: Vec<Value> = if repos_path.exists() {
        let content = std::fs::read_to_string(&repos_path)?;
        serde_json5::from_str(&content).map_err(|e| {
            crate::Error::Decoding(format!("failed to parse excluded_repositories.json5: {e}"))
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
    /// FS paths used for subdirectory pruning inside `walk_directory`.
    pub(crate) fs_paths: Vec<PathBuf>,
    /// Structured list of all excluded entries for feedback reporting.
    pub(crate) entries: Vec<ExcludedEntry>,
}

pub(crate) struct ExcludedEntry {
    pub(crate) source_type: String,
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
            let Some(typ) = e.get("type").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(identity) = json_entry_identity(e) else {
                continue;
            };

            identities.insert(identity.to_owned());
            if typ == "fs" {
                fs_paths.push(PathBuf::from(identity));
            }
            entries.push(ExcludedEntry {
                source_type: typ.to_string(),
                identity: identity.to_owned(),
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

fn handle_repo_exclude_request_inner(
    context: &ServiceRequestContext,
    peppy_dirs: &PeppyDirs,
) -> Result<Payload> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = RepoExcludeRequest::decode(payload.as_ref())?;

    debug!(
        "Received `repo_exclude` request from {sender_instance_id}, source={:?}",
        request.source
    );

    let identity = request.source.identity();
    if identity.trim().is_empty() {
        return RepoExcludeResponse::failure("repository path/URL must not be empty").encode();
    }

    let repos_path = peppy_dirs.conf_dir().join("excluded_repositories.json5");

    let _guard = crate::services::repo::repos_file_lock().lock();

    let mut repos = match read_excluded_repos(peppy_dirs) {
        Ok(repos) => repos,
        Err(e) => return RepoExcludeResponse::failure(e.to_string()).encode(),
    };

    let new_identity = identity.trim();
    let is_duplicate = repos
        .iter()
        .any(|entry| json_entry_identity(entry).is_some_and(|existing| existing == new_identity));

    if is_duplicate {
        return RepoExcludeResponse::failure(format!(
            "repository '{}' already exists",
            new_identity
        ))
        .encode();
    }

    let next_id = repos
        .iter()
        .filter_map(|e| e.get("id").and_then(|v| v.as_u64()))
        .max()
        .map(|max| max + 1)
        .unwrap_or(1);

    repos.push(repo_source_to_json(next_id, &request.source));
    repos.sort_by_key(|e| e.get("id").and_then(|v| v.as_u64()).unwrap_or(0));
    let content = serde_json::to_string_pretty(&repos).map_err(|e| {
        crate::Error::Encoding(format!("failed to serialize excluded repositories: {e}"))
    })?;
    std::fs::write(&repos_path, content)?;

    drop(_guard);

    RepoExcludeResponse::success().encode()
}
