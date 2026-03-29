use super::variant::{resolve_variant, variant_label};
use super::{
    checkout_repo_ref, is_supported_fs_archive, resolve_local_archive_source, sanitize_repo_path,
};
use crate::Result;
use crate::encoding::{NodeInfoRequest, NodeInfoResponse, NodeSource};
use crate::names;
use config::consts::{NODE_CONFIG_FILE, PeppyDirs};
use config::fingerprint::fingerprint_for_bytes;
use config::node::{DEFAULT_VARIANT_NAME, NodeConfig, NodeConfigParser, RawNodeConfig};
use git2::Repository;
use node_stack::NodeStack;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::path::{Path, PathBuf};
use std::{sync::Arc, time::Duration};
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_node_info(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
    timeout: Duration,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        core_node_name,
        instance_id,
        node_name,
        names::NODE_INFO,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| {
                handle_node_info_request(
                    context,
                    Arc::clone(&node_stack),
                    peppy_dirs.clone(),
                    timeout,
                )
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_node_info_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
    timeout: Duration,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id().to_string();

    match tokio::time::timeout(
        timeout,
        handle_node_info_request_inner(&context, node_stack, peppy_dirs),
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
    peppy_dirs: PeppyDirs,
) -> std::result::Result<Payload, String> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = NodeInfoRequest::decode(payload.as_ref()).map_err(|e| format!("{}", e))?;

    debug!("Received `node_info` request from {sender_instance_id}");

    // Resolve the root node config (and keep the source path alive for variant resolution).
    let (node_config, variant_name) = if let Some(ref variant_source) = request.variant {
        let (root_config, root_source_path, cleanup_dir) =
            resolve_node_config_with_source_path(request.source.clone(), &peppy_dirs).await?;
        let _cleanup_guard = super::add::CleanupDir::new(cleanup_dir);

        let label = variant_label(variant_source);
        let resolved =
            resolve_variant(variant_source, &root_config, &root_source_path, &peppy_dirs).await?;
        (resolved.merged_config, Some(label))
    } else {
        let (root_config, root_source_path, cleanup_dir) =
            resolve_node_config_with_source_path(request.source, &peppy_dirs).await?;
        let _cleanup_guard = super::add::CleanupDir::new(cleanup_dir);

        if root_config.has_default_variant() {
            let variant_source = NodeSource::Fs(DEFAULT_VARIANT_NAME.into());
            let label = variant_label(&variant_source);
            let resolved = resolve_variant(
                &variant_source,
                &root_config,
                &root_source_path,
                &peppy_dirs,
            )
            .await?;
            (resolved.merged_config, Some(label))
        } else {
            (
                root_config.into_resolved().map_err(|e| e.to_string())?,
                None,
            )
        }
    };

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
        variant_name,
    )
    .encode()
    .map_err(|e| format!("{}", e))
}

pub async fn resolve_node_config(
    source: NodeSource,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<NodeConfig, String> {
    let (raw, source_path, cleanup_dir) =
        resolve_node_config_with_source_path(source, peppy_dirs).await?;
    let _cleanup_guard = super::add::CleanupDir::new(cleanup_dir);

    if raw.has_default_variant() {
        let variant_source = NodeSource::Fs(DEFAULT_VARIANT_NAME.into());
        let resolved = resolve_variant(&variant_source, &raw, &source_path, peppy_dirs).await?;
        Ok(resolved.merged_config)
    } else {
        raw.into_resolved().map_err(|e| e.to_string())
    }
}

/// Resolves a node config and returns both the config and the source directory path.
/// For git/http sources the returned `Option<PathBuf>` is the temp directory that must
/// be kept alive until the caller is done with the source path.
async fn resolve_node_config_with_source_path(
    source: NodeSource,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<(RawNodeConfig, PathBuf, Option<PathBuf>), String> {
    match source {
        NodeSource::Fs(path) => {
            if is_supported_fs_archive(&path) {
                let resolved = resolve_local_archive_source(&path)?;
                return Ok((
                    resolved.node_config,
                    resolved.source_path,
                    Some(resolved.temp_dir.keep()),
                ));
            }

            let config = parse_node_config_from_fs(&path)?;
            let source_dir = if path.is_dir() {
                path
            } else {
                path.parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."))
            };
            Ok((config, source_dir, None))
        }
        NodeSource::Git {
            repo_url,
            repo_path,
            repo_ref,
        } => parse_node_config_from_git_with_path(repo_url, repo_path, repo_ref).await,
        NodeSource::Http { url } => parse_node_config_from_http_with_path(url, peppy_dirs).await,
    }
}

fn parse_node_config_from_fs(node_path: &Path) -> std::result::Result<RawNodeConfig, String> {
    if is_supported_fs_archive(node_path) {
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

fn parse_node_config_from_archive(
    archive_path: &Path,
) -> std::result::Result<RawNodeConfig, String> {
    let resolved = resolve_local_archive_source(archive_path)?;
    Ok(resolved.node_config)
}

/// Parses a node config from a git repository and returns the source directory path
/// and keeps the temp dir alive by returning it.
async fn parse_node_config_from_git_with_path(
    repo_url: gix_url::Url,
    repo_path: String,
    repo_ref: Option<String>,
) -> std::result::Result<(RawNodeConfig, PathBuf, Option<PathBuf>), String> {
    tokio::task::spawn_blocking(move || {
        let repo_relative_path = sanitize_repo_path(&repo_path)?;

        let temp_dir = tempfile::tempdir()
            .map_err(|e| format!("Failed to create temporary directory: {}", e))?;

        let repo_url_bstring = repo_url.to_bstring();
        let repo_url_str = std::str::from_utf8(repo_url_bstring.as_ref())
            .map_err(|_| "repo_url must be valid UTF-8".to_string())?
            .to_owned();

        let repo = Repository::clone(&repo_url_str, temp_dir.path())
            .map_err(|e| format!("Failed to clone repository: {}", e))?;

        if let Some(repo_ref) = repo_ref.as_deref() {
            checkout_repo_ref(&repo, repo_ref)
                .map_err(|e| format!("Failed to checkout git ref '{}': {}", repo_ref, e))?;
        }

        let candidate_path = temp_dir.path().join(&repo_relative_path);
        let config_path = if candidate_path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json5"))
        {
            candidate_path.clone()
        } else {
            candidate_path.join(NODE_CONFIG_FILE)
        };

        let source_dir = if candidate_path.is_dir() {
            candidate_path
        } else {
            candidate_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| temp_dir.path().to_path_buf())
        };

        let config = NodeConfigParser::from_path(&config_path).map_err(|e| {
            format!(
                "Failed to parse node config at {}: {}",
                config_path.display(),
                e
            )
        })?;

        let checkout_dir = temp_dir.keep();
        Ok((config, source_dir, Some(checkout_dir)))
    })
    .await
    .map_err(|e| format!("Failed to join git clone task: {}", e))?
}

/// Parses a node config from an HTTP source and returns the source directory path
/// and keeps the temp dir alive by returning it.
async fn parse_node_config_from_http_with_path(
    url: url::Url,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<(RawNodeConfig, PathBuf, Option<PathBuf>), String> {
    // For HTTP sources we can use the resolve_http_source from add.rs which already
    // extracts the archive and returns the source path.
    let resolved = super::add::resolve_http_source(&url, peppy_dirs.clone(), None).await?;
    Ok((
        resolved.node_config,
        resolved.source_path,
        resolved.cleanup_dir,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies that `parse_node_config_from_git_with_path` cleans up its temp
    /// directory when an operation (e.g. checking out a nonexistent ref) fails
    /// after the directory has been created and the repo cloned.
    #[tokio::test]
    async fn git_clone_cleans_up_temp_dir_on_checkout_failure() {
        let git_repo_temp_dir = tempfile::TempDir::new().unwrap();
        let git_repo_path = config::test_helpers::create_nodes_git_repo(&git_repo_temp_dir);
        let repo_url =
            gix_url::Url::try_from(git_repo_path.as_path()).expect("git repo path should parse");

        // Snapshot temp dir entries before the call.
        let temp_root = std::env::temp_dir();
        let entries_before: std::collections::BTreeSet<_> = std::fs::read_dir(&temp_root)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();

        // Use a ref that doesn't exist → clone succeeds, checkout_repo_ref fails.
        let result = parse_node_config_from_git_with_path(
            repo_url,
            "nodes/uvc_camera".to_string(),
            Some("nonexistent_ref_that_does_not_exist".to_string()),
        )
        .await;

        assert!(result.is_err(), "should fail with nonexistent git ref");
        assert!(
            result.unwrap_err().contains("Failed to checkout git ref"),
            "error should mention the failed checkout"
        );

        // Verify no temp directories were leaked.
        let entries_after: std::collections::BTreeSet<_> = std::fs::read_dir(&temp_root)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();

        let leaked: Vec<_> = entries_after.difference(&entries_before).collect();
        assert!(
            leaked.is_empty(),
            "temp directory should be cleaned up on error; leaked entries: {:?}",
            leaked
        );
    }
}
