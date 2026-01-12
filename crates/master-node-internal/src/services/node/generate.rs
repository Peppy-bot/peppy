use crate::Result;
use crate::encoding::{NodeGenerateRequest, NodeGenerateResponse};
use crate::names;
use bytes::Bytes;
use config::node::NodeConfigParser;
use node_stack::NodeStack;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::debug;

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

    // Validate dependencies before generation
    let node_config_path = request.node_root_dir.join(config::consts::NODE_CONFIG_FILE);
    if !node_config_path.exists() {
        // Let the generator handle this error
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
            }
            Err(_) => {
                // Let the generator handle config parsing errors
            }
        }
    }

    let build_system = request.build_system;
    let node_root_dir = request.node_root_dir;
    match tokio::task::spawn_blocking(move || {
        generator::generate_lib_for_build_system(build_system, &node_root_dir)
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
