use crate::Result;
use crate::encoding::{NodeInfoRequest, NodeInfoResponse, NodeSource};
use crate::names;
use config::consts::NODE_CONFIG_FILE;
use config::fingerprint::fingerprint_for_bytes;
use config::node::{NodeConfig, NodeConfigParser};
use git2::{Repository, build::CheckoutBuilder};
use node_stack::NodeStack;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::{sync::Arc, time::Duration};
use tar::Archive;
use tokio::task::JoinHandle;
use tracing::debug;
use ureq::Error as HttpError;
use zstd::stream::read::Decoder;

pub async fn listen_for_node_info(
    messenger: &MessengerHandle,
    daemon_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    timeout: Duration,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        daemon_node_name,
        instance_id,
        node_name,
        names::NODE_INFO,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| {
                handle_node_info_request(context, Arc::clone(&node_stack), timeout)
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_node_info_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
    timeout: Duration,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id().to_string();

    match tokio::time::timeout(
        timeout,
        handle_node_info_request_inner(&context, node_stack),
    )
    .await
    {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(reason)) => Err(PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id,
            reason,
        }),
        Err(_) => Err(PeppyError::ServiceTimeout {
            instance_id: None,
            service_name: names::NODE_INFO.to_string(),
        }),
    }
}

async fn handle_node_info_request_inner(
    context: &ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> std::result::Result<Payload, String> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = NodeInfoRequest::decode(payload.as_ref()).map_err(|e| format!("{}", e))?;

    debug!("Received `node_info` request from {sender_instance_id}");

    let node_config = resolve_node_config(request.source).await?;
    let node_name = node_config.manifest.name.as_str();
    let node_tag = node_config.manifest.tag.as_str();

    let (is_in_node_stack, instances_names) = match node_stack.find(node_name, node_tag) {
        Some(entity) => (
            true,
            entity
                .instances()
                .iter()
                .map(|instance| instance.instance_id().as_str().to_owned())
                .collect(),
        ),
        None => (false, Vec::new()),
    };

    let config_json = serde_json5::to_string(&node_config).map_err(|e| format!("{}", e))?;
    let config_integrity = fingerprint_for_bytes(config_json.as_bytes());

    NodeInfoResponse::new(
        node_config,
        is_in_node_stack,
        instances_names,
        config_integrity,
    )
    .encode()
    .map_err(|e| format!("{}", e))
}

pub async fn resolve_node_config(source: NodeSource) -> std::result::Result<NodeConfig, String> {
    match source {
        NodeSource::Fs(path) => parse_node_config_from_fs(&path),
        NodeSource::Git {
            repo_url,
            repo_path,
            repo_ref,
        } => parse_node_config_from_git(repo_url, repo_path, repo_ref).await,
        NodeSource::Http { url } => parse_node_config_from_http(url).await,
    }
}

fn parse_node_config_from_fs(node_path: &Path) -> std::result::Result<NodeConfig, String> {
    if node_path.to_str().is_some_and(|s| s.ends_with(".tar.zst")) {
        return parse_node_config_from_archive(node_path);
    }

    let config_path = if node_path.is_dir() {
        node_path.join(NODE_CONFIG_FILE)
    } else {
        node_path.to_path_buf()
    };

    NodeConfigParser::from_path(&config_path).map_err(|e| {
        format!(
            "Failed to parse node config at {}: {}",
            config_path.display(),
            e
        )
    })
}

fn parse_node_config_from_archive(archive_path: &Path) -> std::result::Result<NodeConfig, String> {
    let file = std::fs::File::open(archive_path)
        .map_err(|e| format!("Failed to open archive {}: {}", archive_path.display(), e))?;
    let decoder = Decoder::new(file)
        .map_err(|e| format!("Failed to decode archive {}: {}", archive_path.display(), e))?;
    let mut archive = Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|e| format!("Failed to read archive {}: {}", archive_path.display(), e))?;

    for entry in entries {
        let mut entry = entry.map_err(|e| {
            format!(
                "Failed to read entry from {}: {}",
                archive_path.display(),
                e
            )
        })?;

        let entry_path = entry
            .path()
            .map_err(|e| {
                format!(
                    "Failed to read entry path from {}: {}",
                    archive_path.display(),
                    e
                )
            })?
            .into_owned();

        if entry.header().entry_type().is_dir() {
            continue;
        }

        if entry_path.file_name() != Some(OsStr::new(NODE_CONFIG_FILE)) {
            continue;
        }

        let mut content = Vec::new();
        entry.read_to_end(&mut content).map_err(|e| {
            format!(
                "Failed to read {} from {}: {}",
                NODE_CONFIG_FILE,
                archive_path.display(),
                e
            )
        })?;

        let config_str = std::str::from_utf8(&content).map_err(|e| {
            format!(
                "{} in {} is not valid UTF-8: {}",
                NODE_CONFIG_FILE,
                archive_path.display(),
                e
            )
        })?;

        return NodeConfigParser::from_content(config_str).map_err(|e| {
            format!(
                "Failed to parse node config from {}: {}",
                archive_path.display(),
                e
            )
        });
    }

    Err(format!(
        "Archive {} does not contain {}",
        archive_path.display(),
        NODE_CONFIG_FILE
    ))
}

fn sanitize_repo_path(repo_path: &str) -> std::result::Result<PathBuf, String> {
    let trimmed = repo_path.trim_start_matches(['/', '\\']);
    let path = PathBuf::from(trimmed);

    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("repo_path must not contain '..'".to_string());
    }

    Ok(path)
}

fn checkout_repo_ref(repo: &Repository, repo_ref: &str) -> std::result::Result<(), git2::Error> {
    let repo_ref = repo_ref.trim();
    if repo_ref.is_empty() {
        return Ok(());
    }

    let object = repo
        .revparse_single(repo_ref)
        .or_else(|_| repo.revparse_single(&format!("refs/tags/{repo_ref}")))
        .or_else(|_| repo.revparse_single(&format!("refs/heads/{repo_ref}")))
        .or_else(|_| repo.revparse_single(&format!("refs/remotes/origin/{repo_ref}")))?;
    let commit = object.peel_to_commit()?;

    repo.set_head_detached(commit.id())?;
    let mut checkout = CheckoutBuilder::new();
    checkout.force();
    repo.checkout_head(Some(&mut checkout))?;
    Ok(())
}

async fn parse_node_config_from_git(
    repo_url: gix_url::Url,
    repo_path: String,
    repo_ref: Option<String>,
) -> std::result::Result<NodeConfig, String> {
    tokio::task::spawn_blocking(move || {
        parse_node_config_from_git_blocking(repo_url, repo_path, repo_ref)
    })
    .await
    .map_err(|e| format!("Failed to join git clone task: {}", e))?
}

fn parse_node_config_from_git_blocking(
    repo_url: gix_url::Url,
    repo_path: String,
    repo_ref: Option<String>,
) -> std::result::Result<NodeConfig, String> {
    let repo_relative_path = sanitize_repo_path(&repo_path)?;

    let checkout_dir =
        tempfile::tempdir().map_err(|e| format!("Failed to create temporary directory: {}", e))?;

    let repo_url_bstring = repo_url.to_bstring();
    let repo_url_str = std::str::from_utf8(repo_url_bstring.as_ref())
        .map_err(|_| "repo_url must be valid UTF-8".to_string())?
        .to_owned();

    let repo = Repository::clone(&repo_url_str, checkout_dir.path())
        .map_err(|e| format!("Failed to clone repository: {}", e))?;

    if let Some(repo_ref) = repo_ref.as_deref() {
        checkout_repo_ref(&repo, repo_ref)
            .map_err(|e| format!("Failed to checkout git ref '{}': {}", repo_ref, e))?;
    }

    let candidate_path = checkout_dir.path().join(&repo_relative_path);
    let config_path = if candidate_path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json5"))
    {
        candidate_path
    } else {
        candidate_path.join(NODE_CONFIG_FILE)
    };

    NodeConfigParser::from_path(&config_path).map_err(|e| {
        format!(
            "Failed to parse node config at {}: {}",
            config_path.display(),
            e
        )
    })
}

fn is_supported_http_archive(url: &url::Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    path.ends_with(".tar.zst") || path.ends_with(".tar.zstd") || path.ends_with(".tzst")
}

async fn parse_node_config_from_http(url: url::Url) -> std::result::Result<NodeConfig, String> {
    tokio::task::spawn_blocking(move || parse_node_config_from_http_blocking(url))
        .await
        .map_err(|e| format!("Failed to join HTTP download task: {}", e))?
}

fn parse_node_config_from_http_blocking(url: url::Url) -> std::result::Result<NodeConfig, String> {
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "HTTP source URL must use http or https (got scheme '{}')",
                other
            ));
        }
    }

    if !is_supported_http_archive(&url) {
        return Err(
            "Only tar.zst (.tar.zstd/.tar.zst/.tzst) archives are supported for HTTP sources"
                .to_string(),
        );
    }

    let response = ureq::get(url.as_str()).call().map_err(|err| {
        let reason = match err {
            HttpError::StatusCode(code) => format!("unexpected status code {code}"),
            other => other.to_string(),
        };
        format!("Failed to download bundle from {}: {}", url, reason)
    })?;

    let reader = response.into_body().into_reader();
    let decoder = Decoder::new(reader)
        .map_err(|e| format!("Failed to decode zstd bundle from {}: {}", url, e))?;
    let mut archive = Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|e| format!("Failed to read tar bundle entries from {}: {}", url, e))?;

    let mut config_candidates: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    let mut top_level_dirs: HashSet<String> = HashSet::new();

    for entry in entries {
        let mut entry =
            entry.map_err(|e| format!("Failed to read tar bundle entry from {}: {}", url, e))?;

        let entry_path = entry
            .path()
            .map_err(|e| format!("Failed to read tar bundle entry path from {}: {}", url, e))?
            .into_owned();

        if entry_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(..)
            )
        }) {
            return Err(format!(
                "Bundle from {} contains unsafe path: {}",
                url,
                entry_path.display()
            ));
        }

        let depth = entry_path.components().count();
        if depth == 1 && entry.header().entry_type().is_dir() {
            if let Some(name) = entry_path.to_str() {
                top_level_dirs.insert(name.to_owned());
            }
        } else if depth >= 2
            && let Some(Component::Normal(first)) = entry_path.components().next()
            && let Some(name) = first.to_str()
        {
            top_level_dirs.insert(name.to_owned());
        }

        if entry.header().entry_type().is_dir() {
            continue;
        }

        if entry_path.file_name() != Some(OsStr::new(NODE_CONFIG_FILE)) {
            continue;
        }

        let mut content = Vec::new();
        entry.read_to_end(&mut content).map_err(|e| {
            format!(
                "Failed to read {} from bundle {}: {}",
                NODE_CONFIG_FILE, url, e
            )
        })?;
        config_candidates.push((entry_path, content));
    }

    let (config_path, config_bytes) = if let Some((path, bytes)) =
        config_candidates.iter().find(|(path, _)| {
            path.components().count() == 1 && path.as_path() == Path::new(NODE_CONFIG_FILE)
        }) {
        (path.clone(), bytes.clone())
    } else if top_level_dirs.len() == 1 {
        let root = top_level_dirs
            .into_iter()
            .next()
            .expect("root dir should exist");
        config_candidates
            .into_iter()
            .find(|(path, _)| {
                let mut comps = path.components();
                let Some(Component::Normal(first)) = comps.next() else {
                    return false;
                };
                let Some(first) = first.to_str() else {
                    return false;
                };
                first == root
                    && comps.next().is_some()
                    && comps.next().is_none()
                    && path.file_name() == Some(OsStr::new(NODE_CONFIG_FILE))
            })
            .ok_or_else(|| {
                format!(
                    "Bundle does not contain {} at the root (or single top-level folder)",
                    NODE_CONFIG_FILE
                )
            })?
    } else {
        return Err(format!(
            "Bundle does not contain {} at the root (or single top-level folder)",
            NODE_CONFIG_FILE
        ));
    };

    let config_str = std::str::from_utf8(&config_bytes).map_err(|e| {
        format!(
            "{} in bundle {} is not valid UTF-8: {}",
            config_path.display(),
            url,
            e
        )
    })?;

    NodeConfigParser::from_content(config_str).map_err(|e| {
        format!(
            "Failed to parse node config from {} in bundle {}: {}",
            config_path.display(),
            url,
            e
        )
    })
}
