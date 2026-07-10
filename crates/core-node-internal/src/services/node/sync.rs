mod codegen;
mod deps;
mod interfaces;
mod pairings;

pub use self::codegen::{AutoSyncParams, auto_sync_if_missing, generate_peppygen_for_node};
pub(crate) use self::interfaces::resolve_contract_doc;
pub use self::interfaces::{collect_all_deployment_interfaces, stack_resolver};

use self::codegen::remove_previous_peppy_dir;
use self::deps::materialize_repo_deps;
use crate::Result;
use crate::services::response::into_service_response;
use config::ParsingError;
use config::node::{NodeConfigParser, validate_dependency_specs};
use core_node_api::ServiceId;
use core_node_api::encoding::{NodeSyncRequest, NodeSyncResponse};
use core_node_api::names;
use daemon_config::consts::PeppyDirs;
use node_stack::NodeStack;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyResult, ServiceMessenger};
use std::collections::{HashMap, HashSet};
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
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        ServiceId::NodeSync.name(),
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

async fn handle_node_sync_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
) -> PeppyResult<Payload> {
    into_service_response(
        &context,
        handle_node_sync_request_inner(&context, &node_stack, peppy_dirs).await,
    )
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
        return NodeSyncResponse::failure("Missing `node_root_dir` in node_sync request")
            .encode()
            .map_err(Into::into);
    }

    if !request.node_root_dir.exists() {
        return NodeSyncResponse::failure(format!(
            "`node_root_dir` does not exist: {}",
            request.node_root_dir.display()
        ))
        .encode()
        .map_err(Into::into);
    }

    if !request.node_root_dir.is_dir() {
        return NodeSyncResponse::failure(format!(
            "`node_root_dir` is not a directory: {}",
            request.node_root_dir.display()
        ))
        .encode()
        .map_err(Into::into);
    }

    // Optional repository fallback. When `include_repositories` is set, walk
    // the dependency tree, fetch every dep absent from the node stack through
    // the repository cache (FS / git / HTTP), and stash the resolved configs
    // so the resolver closure can use them as a second tier. Stack lookups
    // still win.
    let node_config_path = request.node_root_dir.join(config::consts::NODE_CONFIG_FILE);
    let (repo_resolved, repo_resolved_provenance, bfs_stack_hits) = if request.include_repositories
    {
        if !node_config_path.exists() {
            return NodeSyncResponse::failure(format!(
                "Node config file does not exist: {}",
                node_config_path.display()
            ))
            .encode()
            .map_err(Into::into);
        }
        let parsed = match NodeConfigParser::from_path(&node_config_path) {
            Ok(p) => p,
            Err(e) => {
                return NodeSyncResponse::failure(format!("Failed to parse node config: {}", e))
                    .encode()
                    .map_err(Into::into);
            }
        };
        match materialize_repo_deps(&parsed.manifest, node_stack, &peppy_dirs).await {
            Ok((resolved, provenance, stack_hits)) => (resolved, provenance, Some(stack_hits)),
            Err(reason) => {
                return NodeSyncResponse::failure(reason)
                    .encode()
                    .map_err(Into::into);
            }
        }
    } else {
        (HashMap::new(), Vec::new(), None)
    };

    // Resolver closure: node stack first, then any repository-materialized
    // deps. Stack always wins; the repo cache is a fallback opt-in via
    // the request's `include_repositories` flag.
    let resolve_dep = |name: &str, tag: &str| -> Option<config::node::NodeConfig> {
        node_stack
            .find(name, tag)
            .map(|e| e.read().config().clone())
            .or_else(|| {
                repo_resolved
                    .get(&(name.to_owned(), tag.to_owned()))
                    .cloned()
            })
    };

    // Validate dependencies before generation and collect consumed interfaces
    let (consumed_interfaces, language, root_manifest) = if !node_config_path.exists() {
        return NodeSyncResponse::failure(format!(
            "Node config file does not exist: {}",
            node_config_path.display()
        ))
        .encode()
        .map_err(Into::into);
    } else {
        match NodeConfigParser::from_path(&node_config_path) {
            Ok(node_config) => {
                let dep_errors = validate_dependency_specs(
                    &node_config.manifest,
                    &node_config.interfaces,
                    node_config.manifest.name.as_str(),
                    &node_config.manifest.tag,
                    resolve_dep,
                );

                let mut missing_dependencies: HashSet<String> = HashSet::new();
                let mut missing_interfaces: Vec<String> = Vec::new();

                for err in &dep_errors {
                    match err {
                        ParsingError::MissingDependency {
                            dependency,
                            dependency_tag,
                            ..
                        } => {
                            missing_dependencies
                                .insert(format!("{}:{}", dependency, dependency_tag));
                        }
                        ParsingError::MissingInterface(info) => {
                            missing_interfaces.push(format!(
                                "expects {} `{}` from `{}:{}`, but it is not exposed",
                                info.interface_kind.to_lowercase(),
                                info.interface_name,
                                info.dependency,
                                info.dependency_tag
                            ));
                        }
                        ParsingError::UndeclaredLinkId { link_id, .. } => {
                            missing_interfaces
                                .push(format!("references undeclared link_id `{}`", link_id));
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
                    .encode()
                    .map_err(Into::into);
                }

                // Collect consumed interfaces with resolved message formats
                let interface_feedback = |line: &str| {
                    tracing::info!(target: "peppy::interface", "{line}");
                };
                let interfaces = match interfaces::collect_all_deployment_interfaces(
                    &node_config.manifest,
                    &node_config.interfaces,
                    resolve_dep,
                    &peppy_dirs,
                    &interface_feedback,
                ) {
                    Ok(v) => v,
                    Err(reason) => {
                        return NodeSyncResponse::failure(reason)
                            .encode()
                            .map_err(Into::into);
                    }
                };
                let language = node_config.execution.language;
                let root_manifest = node_config.manifest.clone();
                (interfaces, language, root_manifest)
            }
            Err(e) => {
                return NodeSyncResponse::failure(format!("Failed to parse node config: {}", e))
                    .encode()
                    .map_err(Into::into);
            }
        }
    };

    let NodeSyncRequest {
        node_root_dir,
        git_hash,
        include_repositories: _,
    } = request;

    // Provenance: record every dep that resolves through the stack so the
    // response can list them under "Synchronized from node stack:" in the
    // CLI's verbose output. When `include_repositories` is on, the BFS in
    // `materialize_repo_deps` already surfaces direct + transitive stack
    // hits (transitive ones reached through repo-cache-materialized
    // parents); we use that result directly. When the flag is off we
    // can't walk repo-cache nodes, so we fall back to a direct-deps pass
    // over the root manifest.
    let resolved_from_stack: Vec<String> = bfs_stack_hits.unwrap_or_else(|| {
        root_manifest
            .depends_on
            .as_ref()
            .map(|d| {
                let mut acc: Vec<String> = Vec::new();
                let mut seen: HashSet<(String, String)> = HashSet::new();
                for dep in &d.nodes {
                    let name = dep.name.as_str().to_owned();
                    let tag = dep.tag.clone();
                    if !seen.insert((name.clone(), tag.clone())) {
                        continue;
                    }
                    if node_stack.find(&name, &tag).is_some() {
                        acc.push(format!("{}:{}", name, tag));
                    }
                }
                acc
            })
            .unwrap_or_default()
    });

    // Generate peppygen for the root node.
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
                .encode()
                .map_err(Into::into);
        }
        Ok(Err(crate::Error::Io(e))) => {
            return NodeSyncResponse::failure(format!("Failed to write git hash file: {}", e))
                .encode()
                .map_err(Into::into);
        }
        Ok(Err(e)) => {
            return NodeSyncResponse::failure(format!("Failed to sync node: {}", e))
                .encode()
                .map_err(Into::into);
        }
        Err(e) => {
            return NodeSyncResponse::failure(format!(
                "Failed to generate peppygen (generate task failed): {}",
                e
            ))
            .encode()
            .map_err(Into::into);
        }
    };

    NodeSyncResponse::success_with_provenance(resolved_from_stack, repo_resolved_provenance)
        .encode()
        .map_err(Into::into)
}
