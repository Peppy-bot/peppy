use crate::Result;
use crate::names;
use crate::services::repo::refresh::{
    process_refresh, write_cache, write_interface_cache, write_launcher_cache,
};
use crate::services::repo::{
    json_entry_identity, normalize_repo_entries, repo_source_to_json, source_identity,
};
use crate::services::response::into_service_response;
use config::consts::PeppyDirs;
use core_node_api::encoding::{RepoExcludeRequest, RepoExcludeResponse, RepoSourceKind};
use peppylib::messaging::SenderTarget;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyResult, ServiceMessenger};
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
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
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
    let (payload, needs_refresh) = into_service_response(
        &context,
        handle_repo_exclude_request_inner(&context, &peppy_dirs),
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
                warn!("Failed to refresh after repo exclusion: {}", e);
            }
            Err(e) => {
                warn!("Refresh task panicked after repo exclusion: {}", e);
            }
        }
    }

    Ok(payload)
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
    /// FS paths used for subdirectory pruning inside `walk_directory`.
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
            let Some(kind) = e
                .get("type")
                .and_then(|v| v.as_str())
                .and_then(RepoSourceKind::parse)
            else {
                continue;
            };
            let Some(identity) = json_entry_identity(e) else {
                continue;
            };

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

/// Returns `(payload, needs_refresh)`.
fn handle_repo_exclude_request_inner(
    context: &ServiceRequestContext,
    peppy_dirs: &PeppyDirs,
) -> Result<(Payload, bool)> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = RepoExcludeRequest::decode(payload.as_ref())?;

    debug!(
        "Received `repo_exclude` request from {sender_instance_id}, source={:?}",
        request.source
    );

    let identity = source_identity(&request.source);
    if identity.trim().is_empty() {
        return Ok((
            RepoExcludeResponse::failure("repository path/URL must not be empty").encode()?,
            false,
        ));
    }

    let repos_path = peppy_dirs.conf_dir().join("excluded_repositories.json5");

    let _guard = crate::services::repo::repos_file_lock().lock();

    let mut repos = match read_excluded_repos(peppy_dirs) {
        Ok(repos) => repos,
        Err(e) => {
            return Ok((RepoExcludeResponse::failure(e.to_string()).encode()?, false));
        }
    };

    let new_identity = identity.trim();
    let is_duplicate = repos
        .iter()
        .any(|entry| json_entry_identity(entry).is_some_and(|existing| existing == new_identity));

    if is_duplicate {
        return Ok((
            RepoExcludeResponse::failure(format!("repository '{}' already exists", new_identity))
                .encode()?,
            false,
        ));
    }

    let next_id = repos
        .iter()
        .filter_map(|e| e.get("id").and_then(|v| v.as_u64()))
        .max()
        .map(|max| max + 1)
        .unwrap_or(1);

    repos.push(repo_source_to_json(next_id, &request.source));
    repos.sort_by_key(|e| e.get("id").and_then(|v| v.as_u64()).unwrap_or(0));
    let content = json5_pretty::to_string_pretty(&repos).map_err(|e| {
        core_node_api::Error::Encoding(format!("failed to serialize excluded repositories: {e}"))
    })?;
    std::fs::write(&repos_path, content)?;

    drop(_guard);

    Ok((RepoExcludeResponse::success().encode()?, true))
}
