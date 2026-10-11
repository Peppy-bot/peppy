mod codegen;
mod deps;
mod interfaces;
mod pairings;

pub use self::codegen::{AutoSyncParams, auto_sync_if_missing};
pub use self::interfaces::{collect_all_deployment_interfaces, stack_resolver};
pub(crate) use self::interfaces::{resolve_contract_doc, stack_then_repo_resolver};

use self::codegen::remove_previous_peppy_dir;
pub(crate) use self::deps::{DepClosure, MaterializedDeps, materialize_repo_deps};
use super::trace_interface_step;
use crate::Result;
use crate::services::response::into_service_response;
use config::ParsingError;
use config::node::{
    Manifest, MissingDependencyPolicy, NodeConfig, NodeConfigParser, validate_dependency_specs,
};
use core_node_api::ServiceId;
use core_node_api::encoding::{NodeSyncRequest, NodeSyncResponse};
use core_node_api::names;
use daemon_config::consts::PeppyDirs;
use node_stack::NodeStack;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyResult, ServiceMessenger};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
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
    let payload = context.message().payload_bytes();

    let request = NodeSyncRequest::decode(payload.as_ref())?;

    debug!("Received `node_sync` request from {sender_instance_id}");

    let response = match sync_node(request, node_stack, peppy_dirs).await {
        Ok(response) => response,
        Err(reason) => NodeSyncResponse::failure(reason),
    };
    response.encode().map_err(Into::into)
}

/// Generates peppygen for the node of `request`. Returns the response that
/// lists where its dependencies resolved from, or the reason the sync fails.
async fn sync_node(
    request: NodeSyncRequest,
    node_stack: &NodeStack,
    peppy_dirs: PeppyDirs,
) -> std::result::Result<NodeSyncResponse, String> {
    let NodeSyncRequest {
        node_root_dir,
        git_hash,
        include_repositories,
    } = request;

    if node_root_dir.as_os_str().is_empty() {
        return Err("Missing `node_root_dir` in node_sync request".to_owned());
    }
    if !node_root_dir.exists() {
        return Err(format!(
            "`node_root_dir` does not exist: {}",
            node_root_dir.display()
        ));
    }
    if !node_root_dir.is_dir() {
        return Err(format!(
            "`node_root_dir` is not a directory: {}",
            node_root_dir.display()
        ));
    }

    let node_config = parse_node_config(&node_root_dir.join(config::consts::NODE_CONFIG_FILE))?;

    // Optional repository fallback. When `include_repositories` is set, walk
    // the dependency tree, fetch every dep absent from the node stack through
    // the repository cache (FS / git / HTTP), and stash the resolved configs
    // so the resolver closure can use them as a second tier. Stack lookups
    // still win.
    let (repo_resolved, repo_resolved_provenance, walk_stack_hits) = if include_repositories {
        let deps = materialize_repo_deps(
            &node_config.manifest,
            node_stack,
            &peppy_dirs,
            DepClosure::Transitive,
        )
        .await?;
        (deps.configs, deps.provenance, Some(deps.stack_hits))
    } else {
        (HashMap::new(), Vec::new(), None)
    };

    // The repo tier is a fallback opt-in via the request's
    // `include_repositories` flag (`repo_resolved` stays empty without it).
    let resolve_dep = stack_then_repo_resolver(node_stack, &repo_resolved);

    // Validate dependencies before generation.
    let dep_errors = validate_dependency_specs(
        &node_config.manifest,
        &node_config.interfaces,
        node_config.manifest.name.as_str(),
        &node_config.manifest.tag,
        MissingDependencyPolicy::RequireResolvable,
        &resolve_dep,
    );
    if let Some(message) = describe_unresolved_dependencies(&node_config.manifest, &dep_errors) {
        return Err(message);
    }

    // Collect consumed interfaces with resolved message formats
    let consumed_interfaces = interfaces::collect_all_deployment_interfaces(
        &node_config.manifest,
        &node_config.interfaces,
        &resolve_dep,
        &peppy_dirs,
        None,
        &trace_interface_step,
    )?;

    // Provenance: record every dep that resolves through the stack so the
    // response can list them under "Synchronized from node stack:" in the
    // CLI's verbose output. When `include_repositories` is on, the walk in
    // `materialize_repo_deps` already surfaces direct + transitive stack
    // hits (transitive ones reached through repo-cache-materialized
    // parents); we use that result directly. When the flag is off we
    // can't walk repo-cache nodes, so we fall back to a direct-deps pass
    // over the root manifest.
    let resolved_from_stack = walk_stack_hits
        .unwrap_or_else(|| direct_dependencies_in_stack(&node_config.manifest, node_stack));

    // Generate peppygen for the root node.
    let language = node_config.execution.language;
    let generation = tokio::task::spawn_blocking(move || {
        remove_previous_peppy_dir(&node_root_dir);
        generator::generate_peppygen_lib(
            language,
            &node_root_dir,
            consumed_interfaces,
            &git_hash,
            &peppy_dirs,
            generator::CrateDeployMode::default(),
            None,
            generator::NodeTree::Source,
        )
    })
    .await
    .map_err(|e| format!("Failed to generate peppygen (generate task failed): {}", e))?;
    generation.map_err(|e| format!("Failed to generate peppygen: {}", e))?;

    Ok(NodeSyncResponse::success_with_provenance(
        resolved_from_stack,
        repo_resolved_provenance,
    ))
}

/// Parses the `peppy.json5` at `node_config_path`, naming a missing file
/// apart from one that does not parse.
fn parse_node_config(node_config_path: &Path) -> std::result::Result<NodeConfig, String> {
    if !node_config_path.exists() {
        return Err(format!(
            "Node config file does not exist: {}",
            node_config_path.display()
        ));
    }
    NodeConfigParser::from_path(node_config_path)
        .map_err(|e| format!("Failed to parse node config: {}", e))
}

/// The failure message for the dependencies and consumed interfaces of
/// `manifest` that `dep_errors` reports as unresolved, or `None` when every
/// one resolves. Missing dependencies come first, in name order, followed by
/// the interface problems in the order they were found.
fn describe_unresolved_dependencies(
    manifest: &Manifest,
    dep_errors: &[ParsingError],
) -> Option<String> {
    let mut missing_dependencies: BTreeSet<String> = BTreeSet::new();
    let mut missing_interfaces: Vec<String> = Vec::new();

    for err in dep_errors {
        match err {
            ParsingError::MissingDependency {
                dependency,
                dependency_tag,
                ..
            } => {
                missing_dependencies.insert(format!("{}:{}", dependency, dependency_tag));
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
                missing_interfaces.push(format!("references undeclared link_id `{}`", link_id));
            }
            _ => {}
        }
    }

    if missing_dependencies.is_empty() && missing_interfaces.is_empty() {
        return None;
    }

    let mut errors: Vec<String> = Vec::new();
    if !missing_dependencies.is_empty() {
        let verb = match missing_dependencies.len() {
            1 => "it does",
            _ => "they do",
        };
        errors.push(format!(
            "depends on {}, but {} not exist in the stack",
            daemon_config::format_quoted_list(&missing_dependencies),
            verb
        ));
    }
    errors.extend(missing_interfaces);

    Some(format!(
        "`{}:{}` {}",
        manifest.name.as_str(),
        manifest.tag,
        errors.join("; ")
    ))
}

/// The `name:tag` of every direct node dependency of `manifest` that the node
/// stack holds, once each, in declaration order.
fn direct_dependencies_in_stack(manifest: &Manifest, node_stack: &NodeStack) -> Vec<String> {
    let Some(deps) = manifest.depends_on.as_ref() else {
        return Vec::new();
    };
    let mut in_stack: Vec<String> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for dep in &deps.nodes {
        let name = dep.name.as_str().to_owned();
        let tag = dep.tag.clone();
        if !seen.insert((name.clone(), tag.clone())) {
            continue;
        }
        if node_stack.find(&name, &tag).is_some() {
            in_stack.push(format!("{}:{}", name, tag));
        }
    }
    in_stack
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brain_manifest() -> Manifest {
        serde_json5::from_str(r#"{ name: "my_robot_brain", tag: "v1" }"#).expect("manifest parses")
    }

    fn missing_dependency(dependency: &str, dependency_tag: &str) -> ParsingError {
        ParsingError::MissingDependency {
            dependant: "my_robot_brain".to_owned(),
            dependant_tag: "v1".to_owned(),
            dependency: dependency.to_owned(),
            dependency_tag: dependency_tag.to_owned(),
        }
    }

    #[test]
    fn every_dependency_resolving_describes_nothing() {
        assert_eq!(
            describe_unresolved_dependencies(&brain_manifest(), &[]),
            None
        );
    }

    #[test]
    fn one_missing_dependency_reads_in_the_singular() {
        let errors = [missing_dependency("uvc_camera", "v1")];
        assert_eq!(
            describe_unresolved_dependencies(&brain_manifest(), &errors).as_deref(),
            Some(
                "`my_robot_brain:v1` depends on `uvc_camera:v1`, but it does not exist in the \
                 stack"
            )
        );
    }

    #[test]
    fn missing_dependencies_come_once_each_in_name_order_before_interface_problems() {
        let errors = [
            ParsingError::UndeclaredLinkId {
                dependant: "my_robot_brain".to_owned(),
                dependant_tag: "v1".to_owned(),
                link_id: "nonexistent".to_owned(),
            },
            missing_dependency("uvc_camera", "v1"),
            missing_dependency("gps_module", "v2"),
            missing_dependency("uvc_camera", "v1"),
            ParsingError::MissingInterface(Box::new(config::MissingInterface {
                dependant: "my_robot_brain".to_owned(),
                dependant_tag: "v1".to_owned(),
                dependency: "lidar_sensor".to_owned(),
                dependency_tag: "v1".to_owned(),
                interface_kind: "Topic".to_owned(),
                interface_name: "point_cloud".to_owned(),
            })),
        ];
        assert_eq!(
            describe_unresolved_dependencies(&brain_manifest(), &errors).as_deref(),
            Some(
                "`my_robot_brain:v1` depends on `gps_module:v2`, `uvc_camera:v1`, but they do \
                 not exist in the stack; references undeclared link_id `nonexistent`; expects \
                 topic `point_cloud` from `lidar_sensor:v1`, but it is not exposed"
            )
        );
    }
}
