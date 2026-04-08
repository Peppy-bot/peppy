use super::variant::{resolve_variant, variant_label};
use super::{
    checkout_repo_ref, is_supported_fs_archive, resolve_local_archive_source, sanitize_repo_path,
};
use crate::Result;
use crate::encoding::{NodeInfoRequest, NodeInfoResponse, NodeInstanceInfo, NodeSource};
use crate::names;
use config::consts::{NODE_CONFIG_FILE, PeppyDirs};
use config::fingerprint::fingerprint_for_bytes;
use config::node::{DEFAULT_VARIANT_NAME, NodeConfig, NodeConfigParser, ParsedNodeConfig};
use node_stack::InstanceState;
use node_stack::NodeStack;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::path::{Path, PathBuf};
use std::time::Instant;
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

/// Failure mode of `handle_node_info_request_inner`. Routed to a different
/// `PeppyError` variant by the outer wrapper so that lock-poison and other
/// internal faults are not classified as caller-fault `InvalidServiceRequest`.
enum InfoError {
    Invalid(String),
    Internal(String),
}

// Only `String` is convertible into `InfoError` via `?`, and only as the
// `Invalid` (caller-fault) variant. The previous blanket
// `From<E: Display>` swept *every* error type into `Invalid`, which
// silently routed things like serializer faults and lock poisoning to
// `InvalidServiceRequest` instead of `ServiceError`. With this restricted
// impl, internal-fault sites must call `InfoError::Internal(...)` explicitly.
impl From<String> for InfoError {
    fn from(reason: String) -> Self {
        InfoError::Invalid(reason)
    }
}

async fn handle_node_info_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
    timeout: Duration,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id().to_string();

    // Cooperative deadline fires slightly before the outer safety-net timeout,
    // giving the blocking task a chance to abort and clean up resources.
    let deadline = Some(Instant::now() + timeout.saturating_sub(Duration::from_millis(500)));

    match tokio::time::timeout(
        timeout,
        handle_node_info_request_inner(&context, node_stack, peppy_dirs, deadline),
    )
    .await
    {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(InfoError::Invalid(reason))) => Err(PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id,
            reason,
        }),
        Ok(Err(InfoError::Internal(reason))) => Err(PeppyError::ServiceError {
            instance_id: Some(sender_instance_id),
            service_name: names::NODE_INFO.to_string(),
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
    deadline: Option<Instant>,
) -> std::result::Result<Payload, InfoError> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = NodeInfoRequest::decode(payload.as_ref()).map_err(|e| format!("{}", e))?;

    debug!("Received `node_info` request from {sender_instance_id}");

    // Variant resolution errors are collected as issues rather than failing the request,
    // since info calls are display-only and should always return useful information.
    let (root_config, root_source_path, cleanup_dir) =
        resolve_node_config_with_source_path(request.source, &peppy_dirs, deadline).await?;
    let _cleanup_guard = super::add::CleanupDir::new(cleanup_dir);

    let (node_config, variant_name, issues) = if root_config.has_default_variant() {
        let variant_source = NodeSource::Fs(DEFAULT_VARIANT_NAME.into());
        let label = variant_label(&variant_source);
        match resolve_variant(
            &variant_source,
            &root_config,
            &root_source_path,
            &peppy_dirs,
            deadline,
        )
        .await
        {
            Ok(resolved) => (
                merged_config_with_variant_cleanup(resolved),
                Some(label),
                Vec::new(),
            ),
            Err(variant_err) => (
                root_config.into_resolved_or_default(),
                None,
                vec![variant_err],
            ),
        }
    } else {
        (
            root_config.into_resolved().map_err(|e| e.to_string())?,
            None,
            Vec::new(),
        )
    };

    let node_name = node_config.manifest.name.as_str();
    let node_tag = node_config.manifest.tag.as_str();

    let (is_in_node_stack, instances_names, stage, instances, add_log_path, start_log_paths) =
        match node_stack.find(node_name, node_tag) {
            Some(entity) => match entity.read() {
                Ok(guard) => {
                    let stage = Some(guard.stage().name().to_string());
                    let tracked = guard.instances();
                    let instances: Vec<NodeInstanceInfo> = tracked
                        .iter()
                        .map(|instance| NodeInstanceInfo {
                            instance_id: instance.instance_id().as_str().to_owned(),
                            state: match instance.state() {
                                InstanceState::Starting => "starting".to_string(),
                                InstanceState::Running => "running".to_string(),
                            },
                        })
                        .collect();
                    let instances_names: Vec<String> = tracked
                        .iter()
                        .filter(|instance| instance.state() == InstanceState::Running)
                        .map(|instance| instance.instance_id().as_str().to_owned())
                        .collect();
                    let start_log_paths: Vec<PathBuf> = tracked
                        .iter()
                        .map(|instance| {
                            peppy_dirs
                                .logs_dir_start()
                                .join(format!("{}.log", instance.instance_id().as_str()))
                        })
                        .collect();
                    let add_log_path = guard.last_add_log_path().map(Path::to_path_buf);
                    (
                        true,
                        instances_names,
                        stage,
                        instances,
                        add_log_path,
                        start_log_paths,
                    )
                }
                Err(_) => {
                    return Err(InfoError::Internal(format!(
                        "entity {}:{} lock poisoned",
                        node_name, node_tag
                    )));
                }
            },
            None => (false, Vec::new(), None, Vec::new(), None, Vec::new()),
        };

    let config_json = serde_json5::to_string(&node_config)
        .map_err(|e| InfoError::Internal(format!("failed to serialize node config: {}", e)))?;
    let config_integrity = fingerprint_for_bytes(config_json.as_bytes());

    NodeInfoResponse {
        config: node_config,
        is_in_node_stack,
        instances_names,
        config_integrity,
        variant_name,
        issues,
        stage,
        instances,
        add_log_path,
        start_log_paths,
    }
    .encode()
    .map_err(|e| InfoError::Internal(format!("failed to encode NodeInfoResponse: {}", e)))
}

fn merged_config_with_variant_cleanup(resolved: super::variant::ResolvedVariant) -> NodeConfig {
    let _cleanup_guard = super::add::CleanupDir::new(resolved.cleanup_dir);
    resolved.merged_config
}

pub async fn resolve_node_config(
    source: NodeSource,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<NodeConfig, String> {
    let (raw, source_path, cleanup_dir) =
        resolve_node_config_with_source_path(source, peppy_dirs, None).await?;
    let _cleanup_guard = super::add::CleanupDir::new(cleanup_dir);

    if raw.has_default_variant() {
        let variant_source = NodeSource::Fs(DEFAULT_VARIANT_NAME.into());
        let resolved =
            resolve_variant(&variant_source, &raw, &source_path, peppy_dirs, None).await?;
        Ok(merged_config_with_variant_cleanup(resolved))
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
    deadline: Option<Instant>,
) -> std::result::Result<(ParsedNodeConfig, PathBuf, Option<PathBuf>), String> {
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
        } => parse_node_config_from_git_with_path(repo_url, repo_path, repo_ref, deadline).await,
        NodeSource::Http { url, sha256 } => {
            parse_node_config_from_http_with_path(url, sha256, peppy_dirs).await
        }
    }
}

fn parse_node_config_from_fs(node_path: &Path) -> std::result::Result<ParsedNodeConfig, String> {
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
) -> std::result::Result<ParsedNodeConfig, String> {
    let resolved = resolve_local_archive_source(archive_path)?;
    Ok(resolved.node_config)
}

/// Parses a node config from a git repository and returns the source directory path
/// and keeps the temp dir alive by returning it.
async fn parse_node_config_from_git_with_path(
    repo_url: gix_url::Url,
    repo_path: String,
    repo_ref: Option<String>,
    deadline: Option<Instant>,
) -> std::result::Result<(ParsedNodeConfig, PathBuf, Option<PathBuf>), String> {
    tokio::task::spawn_blocking(move || {
        let repo_relative_path = sanitize_repo_path(&repo_path)?;

        let temp_dir = tempfile::tempdir()
            .map_err(|e| format!("Failed to create temporary directory: {}", e))?;

        let repo_url_bstring = repo_url.to_bstring();
        let repo_url_str = std::str::from_utf8(repo_url_bstring.as_ref())
            .map_err(|_| "repo_url must be valid UTF-8".to_string())?
            .to_owned();

        let repo = super::clone_repo_with_deadline(&repo_url_str, temp_dir.path(), deadline)?;

        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err("Git operation timed out".to_string());
        }

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
    expected_sha256: Option<String>,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<(ParsedNodeConfig, PathBuf, Option<PathBuf>), String> {
    // For HTTP sources we can use the resolve_http_source from add.rs which already
    // extracts the archive and returns the source path.
    let resolved =
        super::add::resolve_http_source(&url, peppy_dirs.clone(), expected_sha256).await?;
    Ok((
        resolved.node_config,
        resolved.source_path,
        resolved.cleanup_dir,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Repository, Signature};
    use httptest::{Expectation, Server, matchers::request, responders::status_code};
    use sha2::{Digest, Sha256};
    use std::collections::BTreeSet;
    use std::io::Write;

    fn temp_entries(root: &std::path::Path) -> BTreeSet<PathBuf> {
        std::fs::read_dir(root)
            .unwrap()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect()
    }

    fn contains_config_marker(path: &std::path::Path, marker: &str) -> bool {
        let mut stack = vec![path.to_path_buf()];
        while let Some(current) = stack.pop() {
            let Ok(metadata) = std::fs::metadata(&current) else {
                continue;
            };
            if metadata.is_dir() {
                let Ok(entries) = std::fs::read_dir(&current) else {
                    continue;
                };
                for entry in entries.flatten() {
                    stack.push(entry.path());
                }
                continue;
            }

            if current.file_name().and_then(|name| name.to_str()) == Some(NODE_CONFIG_FILE)
                && let Ok(contents) = std::fs::read_to_string(&current)
                && contents.contains(marker)
            {
                return true;
            }
        }

        false
    }

    fn init_git_repo_with_default_variant(repo_path: &std::path::Path, marker: &str) {
        std::fs::create_dir_all(repo_path).expect("failed to create git repo directory");
        let repo = Repository::init(repo_path).expect("failed to init repository");

        let variant_path = std::path::Path::new("variants/default/peppy.json5");
        if let Some(parent) = variant_path.parent() {
            std::fs::create_dir_all(repo_path.join(parent))
                .expect("failed to create variant directories");
        }

        let variant_config = format!(
            r#"{{
                schema_version: 1,
                execution: {{
                    language: "python",
                    start_cmd: ["python", "{marker}"]
                }}
            }}"#
        );
        std::fs::write(repo_path.join(variant_path), variant_config)
            .expect("failed to write variant config");

        let mut index = repo.index().expect("failed to open index");
        index
            .add_path(variant_path)
            .expect("failed to add variant config");
        index.write().expect("failed to write index");

        let tree_id = index.write_tree().expect("failed to write tree");
        let tree = repo.find_tree(tree_id).expect("failed to find tree");
        let signature =
            Signature::now("Peppy", "peppy@example.com").expect("failed to create signature");
        let commit_id = repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "initial commit",
                &tree,
                &[],
            )
            .expect("failed to create commit");
        let commit = repo.find_commit(commit_id).expect("failed to find commit");
        repo.tag("0.1.0", commit.as_object(), &signature, "0.1.0", false)
            .expect("failed to create tag");
    }

    fn create_http_node_bundle(name: &str, tag: &str) -> Vec<u8> {
        let bundle_dir = tempfile::tempdir().expect("failed to create temp bundle dir");
        let config = format!(
            r#"{{
                schema_version: 1,
                manifest: {{
                    name: "{name}",
                    tag: "{tag}",
                }},
                execution: {{
                    language: "rust",
                    start_cmd: ["sleep", "10"]
                }}
            }}"#
        );
        let manifest_path = bundle_dir.path().join(NODE_CONFIG_FILE);
        std::fs::write(&manifest_path, config).expect("failed to write manifest");

        let mut tar_data = Vec::new();
        {
            let mut tar_builder = tar::Builder::new(&mut tar_data);
            tar_builder
                .append_path_with_name(&manifest_path, NODE_CONFIG_FILE)
                .expect("failed to append manifest to tar");
            tar_builder.finish().expect("failed to finish tar");
        }

        let mut bundle_bytes = Vec::new();
        let mut encoder =
            zstd::Encoder::new(&mut bundle_bytes, 0).expect("failed to create zstd encoder");
        encoder
            .write_all(&tar_data)
            .expect("failed to write compressed bundle");
        encoder.finish().expect("failed to finish encoder");
        bundle_bytes
    }

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
        let entries_before = temp_entries(&temp_root);

        // Use a ref that doesn't exist → clone succeeds, checkout_repo_ref fails.
        let result = parse_node_config_from_git_with_path(
            repo_url,
            "nodes/uvc_camera".to_string(),
            Some("nonexistent_ref_that_does_not_exist".to_string()),
            None,
        )
        .await;

        assert!(result.is_err(), "should fail with nonexistent git ref");
        assert!(
            result.unwrap_err().contains("Failed to checkout git ref"),
            "error should mention the failed checkout"
        );

        // Verify no test-specific temp directories were leaked.
        let marker = "uvc_camera";
        let entries_after = temp_entries(&temp_root);
        let leaked: Vec<_> = entries_after
            .difference(&entries_before)
            .filter(|path| contains_config_marker(path, marker))
            .cloned()
            .collect();
        assert!(
            leaked.is_empty(),
            "temp directory should be cleaned up on error; leaked entries: {:?}",
            leaked
        );
    }

    #[tokio::test]
    async fn resolve_node_config_cleans_up_git_backed_default_variant_checkout() {
        let marker = "default_variant_git_cleanup_marker";
        let variant_repo_root = tempfile::TempDir::new().expect("failed to create temp repo root");
        let variant_repo_path = variant_repo_root.path().join("default_variant_repo");
        init_git_repo_with_default_variant(&variant_repo_path, marker);

        let root_dir = tempfile::TempDir::new().expect("failed to create root node dir");
        let root_config = format!(
            r#"{{
                schema_version: 1,
                manifest: {{
                    name: "default_variant_root",
                    tag: "0.1.0",
                    variants: [
                        {{
                            name: "default",
                            source: {{
                                repo: "{}",
                                path: "variants/default",
                                ref: "0.1.0"
                            }}
                        }}
                    ]
                }},
                interfaces: {{
                    topics: {{
                        emits: [{{ name: "sensor_data" }}]
                    }}
                }}
            }}"#,
            variant_repo_path.display()
        );
        std::fs::write(root_dir.path().join(NODE_CONFIG_FILE), root_config)
            .expect("failed to write root config");

        let peppy_root = tempfile::TempDir::new().expect("failed to create peppy data dir");
        let peppy_dirs = PeppyDirs::new(peppy_root.path());

        let temp_root = std::env::temp_dir();
        let entries_before = temp_entries(&temp_root);

        let resolved =
            resolve_node_config(NodeSource::Fs(root_dir.path().to_path_buf()), &peppy_dirs)
                .await
                .expect("default variant should resolve");

        assert_eq!(
            resolved.execution.language,
            config::node::PeppygenLanguage::Python
        );
        assert!(
            resolved
                .execution
                .start_cmd
                .as_ref()
                .is_some_and(|cmd| cmd.iter().any(|arg| arg == marker)),
            "resolved execution should come from the git-backed default variant"
        );

        let entries_after = temp_entries(&temp_root);
        let leaked: Vec<_> = entries_after
            .difference(&entries_before)
            .filter(|path| contains_config_marker(path, marker))
            .cloned()
            .collect();
        assert!(
            leaked.is_empty(),
            "git-backed default variant checkout should be cleaned up; leaked entries: {:?}",
            leaked
        );
    }

    #[tokio::test]
    async fn resolve_node_config_rejects_http_checksum_mismatch() {
        let bundle = create_http_node_bundle("http_checksum_node", "0.1.0");
        let actual_sha256: String = Sha256::digest(&bundle)
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();
        let wrong_sha256 = if let Some(stripped) = actual_sha256.strip_prefix('0') {
            format!("1{}", stripped)
        } else {
            format!("0{}", &actual_sha256[1..])
        };

        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/bundle.tar.zst"))
                .respond_with(status_code(200).body(bundle)),
        );
        let url = url::Url::parse(&server.url("/bundle.tar.zst").to_string())
            .expect("http bundle url should parse");

        let peppy_root = tempfile::TempDir::new().expect("failed to create peppy data dir");
        let peppy_dirs = PeppyDirs::new(peppy_root.path());

        let error = resolve_node_config(
            NodeSource::Http {
                url,
                sha256: Some(wrong_sha256),
            },
            &peppy_dirs,
        )
        .await
        .expect_err("resolve_node_config should reject checksum mismatch");

        assert!(
            error.contains("checksum mismatch"),
            "expected checksum mismatch, got: {error}"
        );
    }
}
