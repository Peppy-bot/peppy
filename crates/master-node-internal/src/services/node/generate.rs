use crate::Result;
use crate::encoding::{NodeGenerateRequest, NodeGenerateResponse};
use crate::names;
use bytes::Bytes;
use config::node::NodeConfigParser;
use generator::{DeploymentInterface, InterfaceVariant, SubscribedActionMessage};
use node_stack::NodeStack;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::debug;

// TODO: This should delete all the content of the previous `.peppy` folder
pub async fn listen_for_node_generate(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        master_node_node,
        instance_id,
        node_name,
        names::NODE_GENERATE,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(move |context| {
                handle_node_generate_request(context, Arc::clone(&node_stack))
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_node_generate_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_node_generate_request_inner(&context, &node_stack)
        .await
        .map_err(|e| PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        })
}

async fn handle_node_generate_request_inner(
    context: &ServiceRequestContext,
    node_stack: &NodeStack,
) -> Result<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = NodeGenerateRequest::decode(&payload.as_bytes())?;

    debug!("Received `node_generate` request from {sender_instance_id}");

    if request.node_root_dir.as_os_str().is_empty() {
        return NodeGenerateResponse::failure("Missing `node_root_dir` in node_generate request")
            .encode();
    }

    if !request.node_root_dir.exists() {
        return NodeGenerateResponse::failure(format!(
            "`node_root_dir` does not exist: {}",
            request.node_root_dir.display()
        ))
        .encode();
    }

    if !request.node_root_dir.is_dir() {
        return NodeGenerateResponse::failure(format!(
            "`node_root_dir` is not a directory: {}",
            request.node_root_dir.display()
        ))
        .encode();
    }

    // Validate dependencies before generation and collect subscribed interfaces
    let node_config_path = request.node_root_dir.join(config::consts::NODE_CONFIG_FILE);
    let subscribed_interfaces = if !node_config_path.exists() {
        // Let the generator handle this error
        Vec::new()
    } else {
        match NodeConfigParser::from_path(&node_config_path) {
            Ok(node_config) => {
                // Validate dependencies exist in the node stack
                let dependency_specs = node_stack::collect_dependency_specs(&node_config);

                let mut missing_dependencies: HashSet<String> = HashSet::new();
                let mut missing_interfaces: Vec<String> = Vec::new();

                for spec in dependency_specs {
                    // Check if the dependency node exists in the stack
                    if let Some(dependency_entity) =
                        node_stack.find(&spec.node_name, &spec.node_tag)
                    {
                        // Check if the dependency exposes the required interface
                        if !node_stack::exposes_interface(
                            dependency_entity.config(),
                            &spec.interface,
                        ) {
                            missing_interfaces.push(format!(
                                "expects {} `{}` from `{}:{}`, but it is not exposed",
                                spec.interface.kind(),
                                spec.interface.name(),
                                spec.node_name,
                                spec.node_tag
                            ));
                        }
                    } else {
                        // Dependency node doesn't exist in the stack
                        missing_dependencies
                            .insert(format!("{}:{}", spec.node_name, spec.node_tag));
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

                    return NodeGenerateResponse::failure(format!(
                        "`{}:{} {}",
                        node_config.manifest.name.as_str(),
                        node_config.manifest.tag,
                        errors.join("; ")
                    ))
                    .encode();
                }

                // Collect subscribed interfaces with resolved message formats
                collect_subscribed_interfaces(&node_config, node_stack)
            }
            Err(_) => {
                // Let the generator handle config parsing errors
                Vec::new()
            }
        }
    };

    let build_system = request.build_system;
    let node_root_dir = request.node_root_dir;
    match tokio::task::spawn_blocking(move || {
        // Delete the previous .peppy folder to ensure a clean generation
        let peppy_output_dir = node_root_dir.join(config::consts::PEPPY_OUTPUT_DIR);
        if peppy_output_dir.exists() {
            std::fs::remove_dir_all(&peppy_output_dir)?;
        }

        generator::generate_lib_for_build_system(
            build_system,
            &node_root_dir,
            subscribed_interfaces,
        )
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return NodeGenerateResponse::failure(format!("Failed to generate peppygen: {}", e))
                .encode();
        }
        Err(e) => {
            return NodeGenerateResponse::failure(format!(
                "Failed to generate peppygen (generate task failed): {}",
                e
            ))
            .encode();
        }
    };

    NodeGenerateResponse::success().encode()
}

/// Collects subscribed interfaces from a node config and resolves their message formats
/// by looking up the exposed interfaces from dependency nodes in the node stack.
fn collect_subscribed_interfaces(
    node_config: &config::node::NodeConfig,
    node_stack: &NodeStack,
) -> Vec<DeploymentInterface> {
    let mut interfaces = Vec::new();

    let Some(subscribes_to) = &node_config.interfaces.subscribes_to else {
        return interfaces;
    };

    // Collect subscribed topics
    if let Some(topics) = &subscribes_to.topics {
        for subscribed_topic in topics {
            // Find the dependency node in the stack
            if let Some(dependency_entity) =
                node_stack.find(&subscribed_topic.node, &subscribed_topic.tag)
            {
                // Find the exposed topic with the matching name
                if let Some(exposes) = &dependency_entity.config().interfaces.exposes
                    && let Some(exposed_topics) = &exposes.topics
                    && let Some(exposed_topic) = exposed_topics
                        .iter()
                        .find(|t| t.name.trim() == subscribed_topic.name.trim())
                {
                    // Get the message format from the exposed topic
                    if let Some(message_format) = &exposed_topic.message_format {
                        interfaces.push(DeploymentInterface::new(
                            InterfaceVariant::SubscribedTopic(
                                subscribed_topic.clone(),
                                message_format.clone(),
                            ),
                        ));
                    }
                }
            }
        }
    }

    // Collect subscribed services
    if let Some(services) = &subscribes_to.services {
        for subscribed_service in services {
            // Find the dependency node in the stack
            if let Some(dependency_entity) =
                node_stack.find(&subscribed_service.node, &subscribed_service.tag)
            {
                // Find the exposed service with the matching name
                if let Some(exposes) = &dependency_entity.config().interfaces.exposes
                    && let Some(exposed_services) = &exposes.services
                    && let Some(exposed_service) = exposed_services
                        .iter()
                        .find(|s| s.name.trim() == subscribed_service.name.trim())
                {
                    // Get the message formats from the exposed service
                    if let (Some(request_format), Some(response_format)) = (
                        &exposed_service.request_message_format,
                        &exposed_service.response_message_format,
                    ) {
                        interfaces.push(DeploymentInterface::new(
                            InterfaceVariant::SubscribedService(
                                subscribed_service.clone(),
                                request_format.clone(),
                                response_format.clone(),
                            ),
                        ));
                    }
                }
            }
        }
    }

    // Collect subscribed actions
    if let Some(actions) = &subscribes_to.actions {
        for subscribed_action in actions {
            // Find the dependency node in the stack
            if let Some(dependency_entity) =
                node_stack.find(&subscribed_action.node, &subscribed_action.tag)
            {
                // Find the exposed action with the matching name
                if let Some(exposes) = &dependency_entity.config().interfaces.exposes
                    && let Some(exposed_actions) = &exposes.actions
                    && let Some(exposed_action) = exposed_actions
                        .iter()
                        .find(|a| a.name.trim() == subscribed_action.name.trim())
                {
                    // Build the SubscribedActionMessage from exposed action endpoints
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

    interfaces
}
