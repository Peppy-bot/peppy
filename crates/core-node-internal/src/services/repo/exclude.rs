use crate::Result;
use crate::encoding::{RepoExcludeRequest, RepoExcludeResponse, RepoSource};
use crate::names;
use config::consts::PeppyDirs;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;
use tokio::task::JoinHandle;
use tracing::debug;

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

fn repo_source_to_json(id: u64, source: &RepoSource) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("id".to_string(), Value::Number(id.into()));
    match source {
        RepoSource::Fs(path) => {
            map.insert("type".to_string(), Value::String("fs".to_string()));
            map.insert(
                "path".to_string(),
                Value::String(path.to_string_lossy().into_owned()),
            );
        }
        RepoSource::Git { repo_url, repo_ref } => {
            map.insert("type".to_string(), Value::String("git".to_string()));
            map.insert("url".to_string(), Value::String(repo_url.clone()));
            if let Some(r) = repo_ref {
                map.insert("ref".to_string(), Value::String(r.to_string()));
            }
        }
        RepoSource::Url(url) => {
            map.insert("type".to_string(), Value::String("url".to_string()));
            map.insert("url".to_string(), Value::String(url.clone()));
        }
    }
    Value::Object(map)
}

/// Returns the identity string used for duplicate detection.
fn repo_source_identity(source: &RepoSource) -> String {
    match source {
        RepoSource::Fs(path) => path.to_string_lossy().into_owned(),
        RepoSource::Git { repo_url, .. } => repo_url.clone(),
        RepoSource::Url(url) => url.clone(),
    }
}

/// Returns the identity value from a persisted JSON entry (path for fs, url for git/url).
pub(crate) fn json_entry_identity(entry: &Value) -> Option<&str> {
    let typ = entry.get("type")?.as_str()?;
    match typ {
        "fs" => entry.get("path")?.as_str(),
        _ => entry.get("url")?.as_str(),
    }
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

    // Ensure every entry has an integer `id`, auto-assigning when missing.
    let mut max_id: u64 = repos
        .iter()
        .filter_map(|e| e.get("id").and_then(|v| v.as_u64()))
        .max()
        .unwrap_or(0);

    let mut needs_write = false;
    for entry in &mut repos {
        if entry.get("id").and_then(|v| v.as_u64()).is_none() {
            max_id += 1;
            if let Some(obj) = entry.as_object_mut() {
                obj.insert("id".to_string(), Value::Number(max_id.into()));
                needs_write = true;
            }
        }
    }

    // Detect duplicate ids.
    let mut seen_ids = HashSet::new();
    for entry in &repos {
        if let Some(id) = entry.get("id").and_then(|v| v.as_u64())
            && !seen_ids.insert(id)
        {
            return Err(crate::Error::DuplicateRepoId { id });
        }
    }

    // Sort by id so processing order is deterministic.
    repos.sort_by_key(|e| e.get("id").and_then(|v| v.as_u64()).unwrap_or(0));

    if needs_write {
        let content = serde_json::to_string_pretty(&repos).map_err(|e| {
            crate::Error::Encoding(format!("failed to serialize excluded repositories: {e}"))
        })?;
        std::fs::write(&repos_path, content)?;
    }

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
        let raw = read_excluded_repos(peppy_dirs).unwrap_or_default();

        let identities = raw
            .iter()
            .filter_map(|e| json_entry_identity(e).map(|s| s.to_owned()))
            .collect();

        let fs_paths = raw
            .iter()
            .filter(|e| e.get("type").and_then(|v| v.as_str()) == Some("fs"))
            .filter_map(|e| e.get("path").and_then(|v| v.as_str()).map(PathBuf::from))
            .collect();

        let entries = raw
            .iter()
            .filter_map(|e| {
                let typ = e.get("type")?.as_str()?;
                let identity = json_entry_identity(e)?;
                Some(ExcludedEntry {
                    source_type: typ.to_string(),
                    identity: identity.to_owned(),
                })
            })
            .collect();

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

    let identity = repo_source_identity(&request.source);
    if identity.trim().is_empty() {
        return RepoExcludeResponse::failure("repository path/URL must not be empty").encode();
    }

    let repos_path = peppy_dirs.conf_dir().join("excluded_repositories.json5");

    let mut repos = match read_excluded_repos(peppy_dirs) {
        Ok(repos) => repos,
        Err(e) => return RepoExcludeResponse::failure(e.to_string()).encode(),
    };

    // Duplicate check
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

    // Compute the next available id
    let next_id = repos
        .iter()
        .filter_map(|e| e.get("id").and_then(|v| v.as_u64()))
        .max()
        .map(|max| max + 1)
        .unwrap_or(1);

    // Append and write back (JSON is valid JSON5, use pretty for user readability)
    repos.push(repo_source_to_json(next_id, &request.source));
    let content = serde_json::to_string_pretty(&repos).map_err(|e| {
        crate::Error::Encoding(format!("failed to serialize excluded repositories: {e}"))
    })?;
    std::fs::write(&repos_path, content)?;

    RepoExcludeResponse::success().encode()
}
