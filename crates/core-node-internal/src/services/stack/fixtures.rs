//! Planned deployments and placements the stack's unit tests share, so every
//! module tests against the one spelling of a pin, a node and a machine map.

use super::ActiveLaunch;
use super::launch::PlannedDeployment;
use config::AnyType;
use config::runtime::{CoreNodeName, Name};
use core_node_api::{
    InstanceState, NodeStage, SerializedInstance, SerializedNode, SerializedNodeGraph,
};
use daemon_config::launcher::{
    Deployment, DeploymentInstance, DeploymentSource, MemberAddressing, Placements,
};
use daemon_config::repository::{
    DeploymentRoot, EntryOrigin, GitCommit, ItemName, ItemTag, ManifestFingerprint, PinKind,
    PinnedItem, RepoRelativePath,
};
use std::collections::BTreeMap;

/// A parameter a mount path can reference and an instance can leave out.
pub(super) const DEFAULTED_OUTPUT_DIR: &str =
    r#"output_dir: { $type: "string", $default: "/var/lib/peppy_default" }"#;
/// The same parameter with nothing to fall back to, so an instance that
/// omits it cannot resolve its mount path.
pub(super) const REQUIRED_OUTPUT_DIR: &str = r#"output_dir: "string""#;

/// A planned deployment of one container node, with the parameter schema,
/// mount paths and instances a test cares about and defaults everywhere
/// else.
pub(super) fn planned_container_deployment(
    node_name: &str,
    parameters: &str,
    mount_paths: &[&str],
    instances: &[(&str, Option<&str>, BTreeMap<String, AnyType>)],
) -> PlannedDeployment {
    let mounts = mount_paths
        .iter()
        .map(|path| format!("\"{path}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let config = config::node::NodeConfigParser::from_content(&format!(
        r#"{{
            peppy_schema: "node/v1",
            manifest: {{ name: "{node_name}", tag: "v1" }},
            execution: {{
                language: "python",
                container: {{ def_file: "apptainer.def", mount_paths: [{mounts}] }},
                parameters: {{ {parameters} }},
            }},
            interfaces: {{}},
        }}"#
    ))
    .expect("parse node config");

    PlannedDeployment {
        deployment: Deployment {
            source: DeploymentSource::Node {
                name: node_name.to_owned(),
                tag: "v1".to_owned(),
            },
            instances: instances
                .iter()
                .map(|(instance_id, core_node, arguments)| {
                    let mut instance = DeploymentInstance::empty(
                        Name::new(*instance_id).expect("valid instance id"),
                    );
                    instance.arguments = arguments.clone();
                    instance.core_node = core_node.map(str::to_owned);
                    instance
                })
                .collect(),
        },
        node_name: node_name.to_owned(),
        node_tag: "v1".to_owned(),
        config,
        config_sha256: String::new(),
        root: DeploymentRoot::Node(test_root_pin(node_name)),
        closure_pins: Vec::new(),
        pin_manifests: Vec::new(),
        addressing: MemberAddressing::WholeSet,
        targets: daemon_config::launcher::DeploymentTargets::default(),
    }
}

/// A git-backed node pin, the portable shape.
pub(super) fn test_root_pin(node_name: &str) -> PinnedItem {
    PinnedItem {
        kind: PinKind::Node,
        name: ItemName::parse(node_name).expect("valid pin name"),
        tag: ItemTag::parse("v1").expect("valid pin tag"),
        sha256: ManifestFingerprint::parse(&"a".repeat(64)).expect("valid sha"),
        origin: EntryOrigin::Git {
            repo_url: "https://example.com/hub".to_owned(),
            repo_ref: Some("main".to_owned()),
            read_ref: Some(core_node_api::encoding::ReadRef::Named("main".to_owned())),
            commit: GitCommit::parse(&"b".repeat(40)).expect("valid commit"),
            path: RepoRelativePath::parse(&format!("{node_name}/peppy.json5")).expect("valid path"),
        },
    }
}

pub(super) fn placements_with(coordinator: &str, by_instance: &[(&str, &str)]) -> Placements {
    Placements::new(
        CoreNodeName::new(coordinator).expect("valid name"),
        by_instance
            .iter()
            .map(|(instance, core_node)| {
                (
                    (*instance).to_owned(),
                    CoreNodeName::new(*core_node).expect("valid name"),
                )
            })
            .collect(),
    )
}

pub(super) fn planned_deployment(
    node_name: &str,
    instances: &[(&str, Option<&str>)],
) -> PlannedDeployment {
    planned_container_deployment(
        node_name,
        DEFAULTED_OUTPUT_DIR,
        &[],
        &instances
            .iter()
            .map(|(id, core_node)| (*id, *core_node, BTreeMap::new()))
            .collect::<Vec<_>>(),
    )
}

/// A machine's stack graph as its `stack list` reports it: one `holder:v1`
/// node whose instances are `instances`, each in its state.
pub(super) fn graph_holding(instances: &[(&str, InstanceState)]) -> SerializedNodeGraph {
    SerializedNodeGraph {
        nodes: vec![SerializedNode {
            name: "holder".to_owned(),
            tag: "v1".to_owned(),
            core_node: "cn-robot".to_owned(),
            config_path: String::new(),
            artifact_path: None,
            stage: NodeStage::Ready,
            instances: instances
                .iter()
                .map(|(instance_id, state)| {
                    SerializedInstance::of((*instance_id).to_owned(), *state)
                })
                .collect(),
        }],
        edges: Vec::new(),
    }
}

/// A launch `launch-1` whose flat plan runs `instances`, each `(id,
/// machine)`, on the coordinator `cn-desk` and the machines they name, in
/// start order, with no copy.
pub(in crate::services) fn active_launch_running(instances: &[(&str, &str)]) -> ActiveLaunch {
    let deployments: Vec<String> = instances
        .iter()
        .map(|(id, _)| {
            format!(
                r#"{{ source: {{ name: "{id}_node", tag: "v1" }}, instances: [{{ instance_id: "{id}" }}] }}"#
            )
        })
        .collect();
    let document = format!(
        r#"{{ peppy_schema: "launcher/v1", deployments: [{}] }}"#,
        deployments.join(", ")
    );
    let flat = daemon_config::launcher::PeppyLauncherParser::from_content(&document)
        .expect("the plan parses");
    let prepared =
        daemon_config::launcher::PreparedLauncher::load(&flat, std::path::Path::new("x.json5"))
            .expect("it loads");
    let placements = placements_with(
        "cn-desk",
        &instances
            .iter()
            .filter(|(_, machine)| *machine != "cn-desk")
            .copied()
            .collect::<Vec<_>>(),
    );
    let planned = instances
        .iter()
        .map(|(id, _)| planned_deployment(&format!("{id}_node"), &[(id, None)]))
        .collect();
    ActiveLaunch::new(
        "launch-1",
        prepared,
        flat,
        Default::default(),
        placements,
        planned,
    )
}

/// The launch of [`active_launch_running`] with one instance, `instance_id`
/// on `cn-desk`, whose node has the daemon targets `targets`: each with the
/// interface that it names and the scope that the launch gives it.
pub(in crate::services) fn active_launch_with_daemon_targets(
    instance_id: &str,
    targets: &[(
        &str,
        daemon_config::daemon_interface::DaemonInterface,
        serde_json::Value,
    )],
) -> ActiveLaunch {
    let mut active = active_launch_running(&[(instance_id, "cn-desk")]);
    let planned = &mut active.planned[0];
    for (target, interface, scope) in targets {
        planned
            .targets
            .daemon
            .insert((*target).to_owned(), *interface);
        planned.deployment.instances[0]
            .daemon_scopes
            .insert((*target).to_owned(), scope.clone());
    }
    active
}
