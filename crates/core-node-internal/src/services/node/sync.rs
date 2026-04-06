use crate::Result;
use crate::encoding::{NodeSyncRequest, NodeSyncResponse};
use crate::names;
use config::consts::PeppyDirs;
use config::node::{NodeConfigParser, VariantConfigParser};
use config::source::DeploymentSource;
use generator::{ConsumedActionMessage, DeploymentInterface, InterfaceVariant};
use node_stack::NodeStack;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_node_sync(
    messenger: &MessengerHandle,
    core_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        core_node_node,
        instance_id,
        node_name,
        names::NODE_SYNC,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(move |context| {
                handle_node_sync_request(context, Arc::clone(&node_stack), peppy_dirs.clone())
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

/// Safely removes the `.peppy` directory by atomically renaming it first.
///
/// This avoids TOCTOU (Time Of Check, Time Of Use) race conditions that can occur
/// when using `exists()` followed by `remove_dir_all()`. If multiple processes try
/// to generate for the same node concurrently, the simple pattern can fail with
/// "Directory not empty" errors when one process adds files while another is deleting.
///
/// The atomic rename approach:
/// 1. Renames `.peppy` → `.peppy-old-{pid}-{timestamp}` (atomic operation)
/// 2. Spawns background cleanup of the old directory
/// 3. Lets the generator create a fresh `.peppy` directory
fn remove_previous_peppy_dir(node_root_dir: &std::path::Path) {
    let peppy_output_dir = node_root_dir.join(config::consts::PEPPY_OUTPUT_DIR);

    // Safety checks: ensure we're only operating on a `.peppy` directory
    if !peppy_output_dir.exists() {
        return;
    }
    if !peppy_output_dir.is_dir() {
        debug!(
            "Expected .peppy to be a directory, but it's a file: {}",
            peppy_output_dir.display()
        );
        return;
    }
    if peppy_output_dir.file_name() != Some(std::ffi::OsStr::new(config::consts::PEPPY_OUTPUT_DIR))
    {
        debug!(
            "Unexpected directory name, expected {}: {}",
            config::consts::PEPPY_OUTPUT_DIR,
            peppy_output_dir.display()
        );
        return;
    }

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let old_peppy_dir =
        node_root_dir.join(format!(".peppy-old-{}-{}", std::process::id(), timestamp));

    match std::fs::rename(&peppy_output_dir, &old_peppy_dir) {
        Ok(()) => {
            // Best-effort cleanup of old directory in background
            std::thread::spawn(move || {
                std::fs::remove_dir_all(&old_peppy_dir).ok();
            });
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Directory was already removed by another process, that's fine
        }
        Err(e) => {
            // Log warning but proceed - generator will create directories as needed.
            // This handles edge cases like permission issues or concurrent renames.
            debug!(
                "Failed to move old .peppy directory: {}, proceeding with generation",
                e
            );
        }
    }
}

/// Returns `true` when the `.peppy` directory under `node_root_dir` is absent
/// or incomplete and must be (re-)generated.
///
/// A complete `.peppy` directory contains:
/// - `git.hash` (non-empty)
/// - `libs/peppygen/peppy.json5.sha256` (when `has_execution_language` is true)
fn needs_sync(node_root_dir: &std::path::Path, has_execution_language: bool) -> bool {
    let peppy_dir = node_root_dir.join(config::consts::PEPPY_OUTPUT_DIR);
    if !peppy_dir.exists() {
        return true;
    }

    // git.hash must exist and be non-empty
    match std::fs::metadata(peppy_dir.join("git.hash")) {
        Ok(meta) if meta.len() > 0 => {}
        _ => return true,
    }

    // fingerprint file required when execution language is present
    if has_execution_language && !peppy_dir.join("libs/peppygen/peppy.json5.sha256").exists() {
        return true;
    }

    false
}

async fn handle_node_sync_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id();
    handle_node_sync_request_inner(&context, &node_stack, peppy_dirs)
        .await
        .map_err(|e| PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        })
}

async fn handle_node_sync_request_inner(
    context: &ServiceRequestContext,
    node_stack: &NodeStack,
    peppy_dirs: PeppyDirs,
) -> Result<Payload> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = NodeSyncRequest::decode(payload.as_ref())?;

    debug!("Received `node_sync` request from {sender_instance_id}");

    if request.node_root_dir.as_os_str().is_empty() {
        return NodeSyncResponse::failure("Missing `node_root_dir` in node_sync request").encode();
    }

    if !request.node_root_dir.exists() {
        return NodeSyncResponse::failure(format!(
            "`node_root_dir` does not exist: {}",
            request.node_root_dir.display()
        ))
        .encode();
    }

    if !request.node_root_dir.is_dir() {
        return NodeSyncResponse::failure(format!(
            "`node_root_dir` is not a directory: {}",
            request.node_root_dir.display()
        ))
        .encode();
    }

    // Validate dependencies before generation and collect consumed interfaces
    let node_config_path = request.node_root_dir.join(config::consts::NODE_CONFIG_FILE);
    let (
        consumed_interfaces,
        language,
        variants,
        root_manifest,
        root_interfaces,
        root_schema_version,
    ) = if !node_config_path.exists() {
        return NodeSyncResponse::failure(format!(
            "Node config file does not exist: {}",
            node_config_path.display()
        ))
        .encode();
    } else {
        match NodeConfigParser::from_path(&node_config_path) {
            Ok(node_config) => {
                // Validate dependencies exist in the node stack
                let dep_errors = node_stack::validate_dependency_specs(
                    node_config.manifest(),
                    node_config.interfaces(),
                    node_config.manifest_name(),
                    node_config.manifest_tag(),
                    |name, tag| node_stack.find(name, tag).map(|e| e.config().clone()),
                );

                let mut missing_dependencies: HashSet<String> = HashSet::new();
                let mut missing_interfaces: Vec<String> = Vec::new();

                for err in &dep_errors {
                    match err {
                        node_stack::NodeStackError::MissingDependency {
                            dependency,
                            dependency_tag,
                            ..
                        } => {
                            missing_dependencies
                                .insert(format!("{}:{}", dependency, dependency_tag));
                        }
                        node_stack::NodeStackError::MissingInterface {
                            dependency,
                            dependency_tag,
                            interface_kind,
                            interface_name,
                            ..
                        } => {
                            missing_interfaces.push(format!(
                                "expects {} `{}` from `{}:{}`, but it is not exposed",
                                interface_kind.to_lowercase(),
                                interface_name,
                                dependency,
                                dependency_tag
                            ));
                        }
                        node_stack::NodeStackError::UndeclaredLocalNodeId {
                            local_node_id, ..
                        } => {
                            missing_interfaces.push(format!(
                                "references undeclared local_node_id `{}`",
                                local_node_id
                            ));
                        }
                        _ => {}
                    }
                }

                if !missing_dependencies.is_empty() || !missing_interfaces.is_empty() {
                    let mut errors: Vec<String> = Vec::new();

                    if !missing_dependencies.is_empty() {
                        let mut sorted_deps: Vec<_> = missing_dependencies.iter().collect();
                        sorted_deps.sort();
                        errors.push(format!(
                            "depends on {}, but {} not exist in the stack",
                            sorted_deps
                                .iter()
                                .map(|d| format!("`{}`", d))
                                .collect::<Vec<_>>()
                                .join(", "),
                            if missing_dependencies.len() == 1 {
                                "it does"
                            } else {
                                "they do"
                            }
                        ));
                    }

                    for iface_error in missing_interfaces {
                        errors.push(iface_error);
                    }

                    return NodeSyncResponse::failure(format!(
                        "`{}:{} {}",
                        node_config.manifest_name(),
                        node_config.manifest_tag(),
                        errors.join("; ")
                    ))
                    .encode();
                }

                // Collect consumed interfaces with resolved message formats
                let interfaces = collect_consumed_interfaces(
                    node_config.manifest(),
                    node_config.interfaces(),
                    node_stack,
                );
                let language = node_config.execution_language();
                let variants = node_config.manifest().variants.clone();
                let root_manifest = node_config.manifest().clone();
                let root_interfaces = node_config.interfaces().clone();
                let root_schema_version = node_config.schema_version();
                (
                    interfaces,
                    language,
                    variants,
                    root_manifest,
                    root_interfaces,
                    root_schema_version,
                )
            }
            Err(e) => {
                return NodeSyncResponse::failure(format!("Failed to parse node config: {}", e))
                    .encode();
            }
        }
    };

    let NodeSyncRequest {
        node_root_dir,
        git_hash,
    } = request;

    let node_root_dir_for_variants = node_root_dir.clone();
    let git_hash_for_variants = git_hash.clone();
    let consumed_interfaces_for_variants = consumed_interfaces.clone();
    let peppy_dirs_for_variants = peppy_dirs.clone();

    // Generate peppygen for the root node (skip if execution is absent, e.g. default-variant nodes).
    if let Some(language) = language {
        match tokio::task::spawn_blocking(move || -> Result<()> {
            remove_previous_peppy_dir(&node_root_dir);
            generate_peppygen_for_node(
                language,
                &node_root_dir,
                consumed_interfaces,
                &git_hash,
                &peppy_dirs,
                generator::CrateDeployMode::default(),
                None,
            )
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(crate::Error::GeneratorError(e))) => {
                return NodeSyncResponse::failure(format!("Failed to generate peppygen: {}", e))
                    .encode();
            }
            Ok(Err(crate::Error::Io(e))) => {
                return NodeSyncResponse::failure(format!("Failed to write git hash file: {}", e))
                    .encode();
            }
            Ok(Err(e)) => {
                return NodeSyncResponse::failure(format!("Failed to sync node: {}", e)).encode();
            }
            Err(e) => {
                return NodeSyncResponse::failure(format!(
                    "Failed to generate peppygen (generate task failed): {}",
                    e
                ))
                .encode();
            }
        };
    } else {
        // Variant-only node: no peppygen at root, but .peppy/git.hash must
        // still exist alongside the manifest so that `node add` can verify
        // the source is in sync.
        remove_previous_peppy_dir(&node_root_dir);
        let peppy_dir = node_root_dir.join(config::consts::PEPPY_OUTPUT_DIR);
        if let Err(e) = std::fs::create_dir_all(&peppy_dir)
            .and_then(|()| std::fs::write(peppy_dir.join("git.hash"), git_hash.as_bytes()))
        {
            return NodeSyncResponse::failure(format!(
                "Failed to write git hash at root for variant-only node: {}",
                e
            ))
            .encode();
        }
    }

    // Sync variants: each variant gets its own .peppy directory using the root's interfaces
    if let Some(variants) = variants {
        for variant in &variants {
            let DeploymentSource::Local(local) = &variant.source else {
                debug!(
                    "Skipping non-local variant '{}' during sync",
                    variant.name.as_str()
                );
                continue;
            };

            let variant_dir = if local.local.is_relative() {
                node_root_dir_for_variants.join(&local.local)
            } else {
                local.local.clone()
            };
            let variant_dir = variant_dir.canonicalize().unwrap_or(variant_dir);

            if !variant_dir.exists() {
                return NodeSyncResponse::failure(format!(
                    "Variant '{}' source directory does not exist: {}",
                    variant.name.as_str(),
                    variant_dir.display()
                ))
                .encode();
            }

            let variant_config_path = variant_dir.join(config::consts::NODE_CONFIG_FILE);
            let variant_config = match VariantConfigParser::from_path(&variant_config_path) {
                Ok(vc) => vc,
                Err(e) => {
                    return NodeSyncResponse::failure(format!(
                        "Failed to parse variant '{}' config: {}",
                        variant.name.as_str(),
                        e
                    ))
                    .encode();
                }
            };

            let variant_language = variant_config.execution.language;

            // Write a merged NodeConfig (root manifest + root interfaces + variant execution)
            // so the generator can read it as a standard NodeConfig.
            // Strip the variants list — it is no longer relevant once resolved and
            // would trigger a validation error with a "default" variant + execution.
            let mut merged_manifest = root_manifest.clone();
            merged_manifest.variants = None;
            let merged_config = config::node::NodeConfig {
                schema_version: root_schema_version,
                manifest: merged_manifest,
                interfaces: root_interfaces.clone(),
                execution: variant_config.execution,
            };
            let merged_json5 = match serde_json5::to_string(&merged_config) {
                Ok(json) => json,
                Err(e) => {
                    return NodeSyncResponse::failure(format!(
                        "Failed to serialize merged config for variant '{}': {}",
                        variant.name.as_str(),
                        e
                    ))
                    .encode();
                }
            };
            // Write the merged config to a temporary file so the generator can
            // read a full NodeConfig without overwriting the user's peppy.json5.
            let variant_merged_config_file = match tempfile::Builder::new()
                .prefix(".peppy-merged-")
                .suffix(".json5")
                .tempfile()
            {
                Ok(mut f) => {
                    if let Err(e) = std::io::Write::write_all(&mut f, merged_json5.as_bytes()) {
                        return NodeSyncResponse::failure(format!(
                            "Failed to write merged config for variant '{}': {}",
                            variant.name.as_str(),
                            e
                        ))
                        .encode();
                    }
                    f
                }
                Err(e) => {
                    return NodeSyncResponse::failure(format!(
                        "Failed to create temp file for variant '{}': {}",
                        variant.name.as_str(),
                        e
                    ))
                    .encode();
                }
            };
            let variant_merged_config_path = variant_merged_config_file.path().to_path_buf();

            let variant_interfaces = consumed_interfaces_for_variants.clone();
            let variant_git_hash = git_hash_for_variants.clone();
            let variant_peppy_dirs = peppy_dirs_for_variants.clone();

            match tokio::task::spawn_blocking(move || -> Result<()> {
                // Keep the temp file alive for the duration of generation;
                // it is automatically deleted when `_merged_config` is dropped.
                let _merged_config = variant_merged_config_file;
                remove_previous_peppy_dir(&variant_dir);
                generate_peppygen_for_node(
                    variant_language,
                    &variant_dir,
                    variant_interfaces,
                    &variant_git_hash,
                    &variant_peppy_dirs,
                    generator::CrateDeployMode::default(),
                    Some(&variant_merged_config_path),
                )?;
                // Re-fingerprint using the variant's own peppy.json5 so that
                // node-add verification (which reads the variant's config) finds
                // a matching hash.
                config::fingerprint::generate_node_config_fingerprint(
                    variant_dir.join(config::consts::NODE_CONFIG_FILE),
                    variant_dir.join(config::consts::PEPPYGEN_OUTPUT_PATH),
                )
                .map_err(generator::GeneratorError::Config)?;
                Ok(())
            })
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    return NodeSyncResponse::failure(format!(
                        "Failed to generate peppygen for variant '{}': {}",
                        variant.name.as_str(),
                        e
                    ))
                    .encode();
                }
                Err(e) => {
                    return NodeSyncResponse::failure(format!(
                        "Failed to generate peppygen for variant '{}' (task failed): {}",
                        variant.name.as_str(),
                        e
                    ))
                    .encode();
                }
            }
        }
    }

    NodeSyncResponse::success().encode()
}

/// Builds a lookup from `local_id` → `(dep_name, dep_tag)` using the node's `depends_on.nodes`.
fn build_dependency_lookup(
    manifest: &config::node::Manifest,
) -> std::collections::HashMap<String, (String, String)> {
    manifest
        .depends_on
        .as_ref()
        .map(|d| {
            d.nodes
                .iter()
                .map(|n| {
                    (
                        n.local_id.clone(),
                        (n.name.as_str().to_string(), n.tag.clone()),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Collects consumed interfaces from a node config and resolves their message formats
/// by looking up the exposed interfaces from dependency nodes in the node stack.
pub fn collect_consumed_interfaces(
    manifest: &config::node::Manifest,
    interfaces_cfg: &config::node::Interfaces,
    node_stack: &NodeStack,
) -> Vec<DeploymentInterface> {
    let mut interfaces = Vec::new();
    let dep_lookup = build_dependency_lookup(manifest);

    // Collect consumed topics
    if let Some(topic_interfaces) = &interfaces_cfg.topics
        && let Some(consumed_topics) = &topic_interfaces.consumes
    {
        for consumed_topic in consumed_topics {
            match consumed_topic {
                config::node::ConsumedTopic::Linked(linked) => {
                    let Some((dep_name, dep_tag)) = dep_lookup.get(linked.local_node_id.as_str())
                    else {
                        continue;
                    };
                    if let Some(dependency_entity) = node_stack.find(dep_name, dep_tag)
                        && let Some(dep_topics) = &dependency_entity.config().interfaces.topics
                        && let Some(emitted_topics) = &dep_topics.emits
                        && let Some(emitted_topic) = emitted_topics
                            .iter()
                            .find(|t| t.name.trim() == linked.name.trim())
                        && let Some(message_format) = &emitted_topic.message_format
                    {
                        interfaces.push(DeploymentInterface::new(
                            InterfaceVariant::ConsumedTopic {
                                topic: consumed_topic.clone(),
                                message_format: message_format.clone(),
                                dependency_node_name: dep_name.clone(),
                            },
                        ));
                    }
                }
                config::node::ConsumedTopic::External(external) => {
                    interfaces.push(DeploymentInterface::new(
                        InterfaceVariant::ExternalConsumedTopic {
                            name: external.name.clone(),
                            message_format: external.message_format.clone(),
                        },
                    ));
                }
            }
        }
    }

    // Collect consumed services
    if let Some(service_interfaces) = &interfaces_cfg.services
        && let Some(consumed_services) = &service_interfaces.consumes
    {
        for consumed_service in consumed_services {
            let Some((dep_name, dep_tag)) = dep_lookup.get(&consumed_service.local_node_id) else {
                continue;
            };
            if let Some(dependency_entity) = node_stack.find(dep_name, dep_tag)
                && let Some(dep_services) = &dependency_entity.config().interfaces.services
                && let Some(exposed_services) = &dep_services.exposes
                && let Some(exposed_service) = exposed_services
                    .iter()
                    .find(|s| s.name.trim() == consumed_service.name.trim())
            {
                interfaces.push(DeploymentInterface::new(
                    InterfaceVariant::ConsumedService {
                        service: consumed_service.clone(),
                        request_format: exposed_service
                            .request_message_format
                            .clone()
                            .unwrap_or_default(),
                        response_format: exposed_service
                            .response_message_format
                            .clone()
                            .unwrap_or_default(),
                        dependency_node_name: dep_name.clone(),
                    },
                ));
            }
        }
    }

    // Collect consumed actions
    if let Some(action_interfaces) = &interfaces_cfg.actions
        && let Some(consumed_actions) = &action_interfaces.consumes
    {
        for consumed_action in consumed_actions {
            let Some((dep_name, dep_tag)) = dep_lookup.get(&consumed_action.local_node_id) else {
                continue;
            };
            if let Some(dependency_entity) = node_stack.find(dep_name, dep_tag)
                && let Some(dep_actions) = &dependency_entity.config().interfaces.actions
                && let Some(exposed_actions) = &dep_actions.exposes
                && let Some(exposed_action) = exposed_actions
                    .iter()
                    .find(|a| a.name.trim() == consumed_action.name.trim())
            {
                let action_message = ConsumedActionMessage {
                    goal_request: exposed_action
                        .goal_service
                        .as_ref()
                        .and_then(|s| s.request_message_format.clone()),
                    goal_response: exposed_action
                        .goal_service
                        .as_ref()
                        .and_then(|s| s.response_message_format.clone()),
                    feedback: exposed_action
                        .feedback_topic
                        .as_ref()
                        .and_then(|t| t.message_format.clone()),
                    result_request: exposed_action
                        .result_service
                        .as_ref()
                        .and_then(|s| s.request_message_format.clone()),
                    result_response: exposed_action
                        .result_service
                        .as_ref()
                        .and_then(|s| s.response_message_format.clone()),
                };

                interfaces.push(DeploymentInterface::new(InterfaceVariant::ConsumedAction {
                    action: consumed_action.clone(),
                    messages: action_message,
                    dependency_node_name: dep_name.clone(),
                }));
            }
        }
    }

    interfaces
}

/// Parameters for [`auto_sync_if_missing`].
pub struct AutoSyncParams<'a> {
    pub node_dir: &'a std::path::Path,
    pub execution_language: Option<config::node::PeppygenLanguage>,
    pub manifest: &'a config::node::Manifest,
    pub interfaces: &'a config::node::Interfaces,
    pub git_hash: &'a str,
    pub variant: Option<AutoSyncVariant<'a>>,
}

pub struct AutoSyncVariant<'a> {
    pub dir: &'a std::path::Path,
    pub language: config::node::PeppygenLanguage,
    /// The fully merged node config (root manifest + variant execution).
    /// Needed because the variant's own `peppy.json5` lacks a `manifest`.
    pub merged_config: &'a config::node::NodeConfig,
}

/// Auto-generates the `.peppy` directory for a node that has never been synced.
///
/// When the `.peppy` directory is entirely absent (e.g. fresh clone), this
/// function generates peppygen (if the node has an execution block) or writes
/// just the `git.hash` file (variant-only nodes without execution at root).
///
/// If a variant is provided and its `.peppy` directory is also absent,
/// generates peppygen for the variant and re-fingerprints using the variant's
/// own `peppy.json5`.
///
/// Directories whose `.peppy` already exists and contains all required files
/// are skipped (no-op). If `.peppy` exists but is incomplete (e.g. missing
/// `git.hash` or the peppygen fingerprint), it is removed and regenerated.
pub fn auto_sync_if_missing(
    params: AutoSyncParams<'_>,
    node_stack: &NodeStack,
    peppy_dirs: &PeppyDirs,
) -> crate::Result<()> {
    // Sync root
    let peppy_dir = params.node_dir.join(config::consts::PEPPY_OUTPUT_DIR);
    if needs_sync(params.node_dir, params.execution_language.is_some()) {
        remove_previous_peppy_dir(params.node_dir);
        if let Some(language) = params.execution_language {
            let consumed =
                collect_consumed_interfaces(params.manifest, params.interfaces, node_stack);
            generate_peppygen_for_node(
                language,
                params.node_dir,
                consumed,
                params.git_hash,
                peppy_dirs,
                generator::CrateDeployMode::default(),
                None,
            )?;
        } else {
            // Variant-only node (no execution at root): just write git.hash
            std::fs::create_dir_all(&peppy_dir)
                .and_then(|()| {
                    std::fs::write(peppy_dir.join("git.hash"), params.git_hash.as_bytes())
                })
                .map_err(crate::Error::from)?;
        }
    }

    // Sync variant
    if let Some(v) = params.variant
        && needs_sync(v.dir, true)
    {
        remove_previous_peppy_dir(v.dir);
        // Write the merged config (root manifest + variant execution) to a
        // temp file so the generator can parse a full NodeConfig. The
        // variant's own peppy.json5 lacks a `manifest` field.
        // Strip variant declarations from the manifest to avoid the
        // ExecutionWithDefaultVariant validation error when the generator
        // re-parses the config.
        let mut config_for_gen = v.merged_config.clone();
        config_for_gen.manifest.variants = None;
        let merged_json5 = serde_json5::to_string(&config_for_gen)
            .map_err(|e| crate::Error::Io(std::io::Error::other(e)))?;
        let mut tmp = tempfile::Builder::new()
            .prefix(".peppy-merged-")
            .suffix(".json5")
            .tempfile()
            .map_err(crate::Error::from)?;
        std::io::Write::write_all(&mut tmp, merged_json5.as_bytes()).map_err(crate::Error::from)?;
        let merged_config_path = tmp.path().to_path_buf();

        let consumed = collect_consumed_interfaces(params.manifest, params.interfaces, node_stack);
        generate_peppygen_for_node(
            v.language,
            v.dir,
            consumed,
            params.git_hash,
            peppy_dirs,
            generator::CrateDeployMode::default(),
            Some(&merged_config_path),
        )?;

        // Re-fingerprint using the variant's own peppy.json5 so that
        // node-add verification (which reads the variant's config) finds
        // a matching hash.
        config::fingerprint::generate_node_config_fingerprint(
            v.dir.join(config::consts::NODE_CONFIG_FILE),
            v.dir.join(config::consts::PEPPYGEN_OUTPUT_PATH),
        )
        .map_err(generator::GeneratorError::Config)?;
    }

    Ok(())
}

/// Generates the peppygen library for a node.
///
/// This function takes the pre-collected data and generates the peppygen
/// library in the node directory. Use `collect_consumed_interfaces` to
/// gather the consumed interfaces before calling this function.
///
/// This function is designed to be called from within `spawn_blocking` contexts
/// where the data has already been extracted and can be moved into the closure.
pub fn generate_peppygen_for_node(
    language: config::node::PeppygenLanguage,
    node_dir: impl AsRef<std::path::Path>,
    consumed_interfaces: Vec<DeploymentInterface>,
    git_hash: &str,
    peppy_dirs: &PeppyDirs,
    deploy_mode: generator::CrateDeployMode,
    config_path: Option<&std::path::Path>,
) -> crate::Result<()> {
    generator::generate_peppygen_lib(
        language,
        node_dir,
        consumed_interfaces,
        git_hash,
        peppy_dirs,
        deploy_mode,
        config_path,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_sync_returns_true_when_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(needs_sync(tmp.path(), true));
        assert!(needs_sync(tmp.path(), false));
    }

    #[test]
    fn needs_sync_returns_true_when_git_hash_missing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".peppy")).unwrap();
        assert!(needs_sync(tmp.path(), false));
    }

    #[test]
    fn needs_sync_returns_true_when_git_hash_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy = tmp.path().join(".peppy");
        std::fs::create_dir_all(&peppy).unwrap();
        std::fs::write(peppy.join("git.hash"), b"").unwrap();
        assert!(needs_sync(tmp.path(), false));
    }

    #[test]
    fn needs_sync_returns_true_when_fingerprint_missing_with_execution_language() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy = tmp.path().join(".peppy");
        std::fs::create_dir_all(&peppy).unwrap();
        std::fs::write(peppy.join("git.hash"), b"abc123").unwrap();
        // has_execution_language = true but no fingerprint file
        assert!(needs_sync(tmp.path(), true));
    }

    #[test]
    fn needs_sync_returns_false_when_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy = tmp.path().join(".peppy");
        let peppygen = peppy.join("libs/peppygen");
        std::fs::create_dir_all(&peppygen).unwrap();
        std::fs::write(peppy.join("git.hash"), b"abc123").unwrap();
        std::fs::write(peppygen.join("peppy.json5.sha256"), b"deadbeef").unwrap();
        assert!(!needs_sync(tmp.path(), true));
    }

    #[test]
    fn needs_sync_ignores_fingerprint_when_no_execution_language() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy = tmp.path().join(".peppy");
        std::fs::create_dir_all(&peppy).unwrap();
        std::fs::write(peppy.join("git.hash"), b"abc123").unwrap();
        // No fingerprint file, but has_execution_language = false
        assert!(!needs_sync(tmp.path(), false));
    }
}
