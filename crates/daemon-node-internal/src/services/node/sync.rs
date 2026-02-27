use crate::Result;
use crate::encoding::{NodeSyncRequest, NodeSyncResponse};
use crate::names;
use config::consts::PeppyDirs;
use config::node::NodeConfigParser;
use generator::{DeploymentInterface, InterfaceVariant, SubscribedActionMessage};
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
    daemon_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        daemon_node_node,
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

    // Validate dependencies before generation and collect subscribed interfaces
    let node_config_path = request.node_root_dir.join(config::consts::NODE_CONFIG_FILE);
    let (subscribed_interfaces, language) = if !node_config_path.exists() {
        return NodeSyncResponse::failure(format!(
            "Node config file does not exist: {}",
            node_config_path.display()
        ))
        .encode();
    } else {
        match NodeConfigParser::from_path(&node_config_path) {
            Ok(node_config) => {
                // Validate dependencies and peers exist in the node stack
                let resolve =
                    |name: &str, tag: &str| node_stack.find(name, tag).map(|e| e.config().clone());
                let dep_errors = node_stack::validate_dependency_specs(
                    &node_config,
                    node_config.manifest.name.as_str(),
                    &node_config.manifest.tag,
                    resolve,
                );
                let peer_errors = node_stack::validate_peer_specs(
                    &node_config,
                    node_config.manifest.name.as_str(),
                    &node_config.manifest.tag,
                    resolve,
                );

                let mut missing_dependencies: HashSet<String> = HashSet::new();
                let mut missing_interfaces: Vec<String> = Vec::new();

                for err in dep_errors.iter().chain(peer_errors.iter()) {
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
                        node_config.manifest.name.as_str(),
                        node_config.manifest.tag,
                        errors.join("; ")
                    ))
                    .encode();
                }

                // Collect subscribed interfaces with resolved message formats
                let interfaces = collect_subscribed_interfaces(&node_config, node_stack);
                let language = node_config.manifest.language;
                (interfaces, language)
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

    match tokio::task::spawn_blocking(move || -> Result<()> {
        remove_previous_peppy_dir(&node_root_dir);
        generate_peppygen_for_node(
            language,
            &node_root_dir,
            subscribed_interfaces,
            &git_hash,
            &peppy_dirs,
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

    NodeSyncResponse::success().encode()
}

/// Resolves topic interfaces from a list of subscribed topics by looking up
/// exposed message formats from dependency/peer nodes in the node stack.
fn resolve_topic_interfaces(
    topics: &[config::node::SubscribedTopic],
    node_stack: &NodeStack,
    interfaces: &mut Vec<DeploymentInterface>,
) {
    for subscribed_topic in topics {
        if let Some(dependency_entity) =
            node_stack.find(&subscribed_topic.node, &subscribed_topic.tag)
            && let Some(exposes) = &dependency_entity.config().interfaces.exposes
            && let Some(exposed_topics) = &exposes.topics
            && let Some(exposed_topic) = exposed_topics
                .iter()
                .find(|t| t.name.trim() == subscribed_topic.name.trim())
            && let Some(message_format) = &exposed_topic.message_format
        {
            interfaces.push(DeploymentInterface::new(InterfaceVariant::SubscribedTopic(
                subscribed_topic.clone(),
                message_format.clone(),
            )));
        }
    }
}

/// Resolves service interfaces from a list of subscribed services by looking up
/// exposed request/response formats from dependency/peer nodes in the node stack.
fn resolve_service_interfaces(
    services: &[config::node::SubscribedService],
    node_stack: &NodeStack,
    interfaces: &mut Vec<DeploymentInterface>,
) {
    for subscribed_service in services {
        if let Some(dependency_entity) =
            node_stack.find(&subscribed_service.node, &subscribed_service.tag)
            && let Some(exposes) = &dependency_entity.config().interfaces.exposes
            && let Some(exposed_services) = &exposes.services
            && let Some(exposed_service) = exposed_services
                .iter()
                .find(|s| s.name.trim() == subscribed_service.name.trim())
        {
            interfaces.push(DeploymentInterface::new(
                InterfaceVariant::SubscribedService(
                    subscribed_service.clone(),
                    exposed_service
                        .request_message_format
                        .clone()
                        .unwrap_or_default(),
                    exposed_service
                        .response_message_format
                        .clone()
                        .unwrap_or_default(),
                ),
            ));
        }
    }
}

/// Collects subscribed interfaces from a node config and resolves their message formats
/// by looking up the exposed interfaces from dependency nodes in the node stack.
/// Also collects `peers_with` interfaces using the same resolution logic.
pub fn collect_subscribed_interfaces(
    node_config: &config::node::NodeConfig,
    node_stack: &NodeStack,
) -> Vec<DeploymentInterface> {
    let mut interfaces = Vec::new();

    // Collect subscribes_to interfaces (hard dependencies)
    if let Some(subscribes_to) = &node_config.interfaces.subscribes_to {
        if let Some(topics) = &subscribes_to.topics {
            resolve_topic_interfaces(topics, node_stack, &mut interfaces);
        }

        if let Some(services) = &subscribes_to.services {
            resolve_service_interfaces(services, node_stack, &mut interfaces);
        }

        // Collect subscribed actions
        if let Some(actions) = &subscribes_to.actions {
            for subscribed_action in actions {
                if let Some(dependency_entity) =
                    node_stack.find(&subscribed_action.node, &subscribed_action.tag)
                    && let Some(exposes) = &dependency_entity.config().interfaces.exposes
                    && let Some(exposed_actions) = &exposes.actions
                    && let Some(exposed_action) = exposed_actions
                        .iter()
                        .find(|a| a.name.trim() == subscribed_action.name.trim())
                {
                    let action_message = SubscribedActionMessage {
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

                    interfaces.push(DeploymentInterface::new(
                        InterfaceVariant::SubscribedAction(
                            subscribed_action.clone(),
                            action_message,
                        ),
                    ));
                }
            }
        }
    }

    // Collect peers_with interfaces (no dependency edges, same resolution logic)
    if let Some(peers_with) = &node_config.interfaces.peers_with {
        if let Some(topics) = &peers_with.topics {
            resolve_topic_interfaces(topics, node_stack, &mut interfaces);
        }

        if let Some(services) = &peers_with.services {
            resolve_service_interfaces(services, node_stack, &mut interfaces);
        }
    }

    interfaces
}

/// Generates the peppygen library for a node.
///
/// This function takes the pre-collected data and generates the peppygen
/// library in the node directory. Use `collect_subscribed_interfaces` to
/// gather the subscribed interfaces before calling this function.
///
/// This function is designed to be called from within `spawn_blocking` contexts
/// where the data has already been extracted and can be moved into the closure.
pub fn generate_peppygen_for_node(
    language: config::node::PeppygenLanguage,
    node_dir: impl AsRef<std::path::Path>,
    subscribed_interfaces: Vec<DeploymentInterface>,
    git_hash: &str,
    peppy_dirs: &PeppyDirs,
) -> crate::Result<()> {
    generator::generate_peppygen_lib(
        language,
        node_dir,
        subscribed_interfaces,
        git_hash,
        peppy_dirs,
    )?;

    Ok(())
}
