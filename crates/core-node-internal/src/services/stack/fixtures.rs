//! Planned deployments, placements and launches the stack's unit tests
//! share, so every module tests against the one spelling of a pin, a node, a
//! machine map and a launcher of copies.

use super::ActiveLaunch;
use super::launch::resolve::{LaunchPlacements, resolve_placements};
use super::launch::{NodeKey, PlannedDeployment};
use config::AnyType;
use config::runtime::{CopyName, CoreNodeName, Name};
use core_node_api::encoding::{
    CopyReference, LaunchGoal, LaunchWord, LauncherOrigin, PlacementSpec, StackBudgets,
};
use core_node_api::{
    InstanceState, NodeStage, SerializedInstance, SerializedNode, SerializedNodeGraph,
};
use daemon_config::launcher::{
    ComposedLaunch, Deployment, DeploymentInstance, DeploymentSource, MemberAddressing, Placements,
    PreparedLauncher,
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
            manifest: {{ name: "{node_name}", tag: "v1", description: ["A test node."] }},
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

/// A launcher whose `robot` axis runs as copies of `arm` and `spare`. An
/// `arm` is an arm, its base, a commander, a part its own axis `commander`
/// brings, and a brain, a part its own axis `brain` brings when a word
/// selects `ai_brain_vla`. A `spare` is an arm alone. The file lists the copy
/// `bravo` of `arm`.
pub(super) fn robots() -> PreparedLauncher {
    let document = r#"{
        peppy_schema: "launcher/v1",
        description: ["A test launcher."],
        components: [
            { name: "robot", cardinality: "zero_or_more", options: {
                arm: {
                    deployments: [
                        { source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] },
                        { commander: "pad" },
                    ],
                    components: [
                        { name: "commander", options: {
                            pad: { deployments: [
                                { source: { name: "pad", tag: "v1" }, instances: [
                                    { instance_id: "commander_inst", links: { arm: "arm_inst" } }
                                ] }
                            ] },
                        } },
                        { name: "brain", cardinality: "zero_or_one", options: {
                            ai_brain_vla: { deployments: [
                                { source: { name: "vla", tag: "v1" }, instances: [{ instance_id: "brain_inst" }] }
                            ] },
                        } },
                    ],
                },
                spare: { deployments: [
                    { source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] },
                ] },
            } },
        ],
        deployments: [{ robot: "arm", instances: [{ instance_id: "bravo" }] }],
    }"#;
    let parsed = daemon_config::launcher::PeppyLauncherParser::from_content(document)
        .expect("the launcher parses");
    PreparedLauncher::load(&parsed, std::path::Path::new("robots.json5")).expect("it loads")
}

/// The launch of [`robots`] on the coordinator `cn-desk` with the copy
/// words `words`, the joins `joins` and `placement`: what it composes, and
/// the machine of each instance and of each copy.
pub(in crate::services::stack) fn launch_of_robots(
    placement: PlacementSpec,
    words: &[&str],
    joins: &[&str],
) -> (ComposedLaunch, LaunchPlacements) {
    let words: Vec<LaunchWord> = words.iter().map(|word| word.parse().unwrap()).collect();
    let joins: Vec<CopyReference> = joins.iter().map(|join| join.parse().unwrap()).collect();
    let composed = robots()
        .launch(&words, &joins)
        .expect("the launch composes");
    let goal = LaunchGoal::new(
        LauncherOrigin::repository("robots"),
        "launch-1",
        StackBudgets::new(1, 1, 1, None),
    )
    .with_layout(placement, words, joins)
    .expect("the layout holds");
    let placements =
        resolve_placements(&composed, &goal, "cn-desk").expect("the placements resolve");
    (composed, placements)
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
        r#"{{ peppy_schema: "launcher/v1", description: ["A test launcher."], deployments: [{}] }}"#,
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

/// `active`, which runs on `cn-desk` alone, after the launch of [`robots`]
/// with the copy words `words` and the joins `joins` recorded the copies it
/// starts, each on its machine with each part on the machine its word gives
/// it, and after a removal took each of `removed` off the stack.
pub(in crate::services) fn with_launched_copies(
    mut active: ActiveLaunch,
    words: &[&str],
    joins: &[&str],
    removed: &[&str],
) -> ActiveLaunch {
    assert!(
        active.placements.coordinator() == "cn-desk" && !active.placements.is_federated(),
        "the placements of the launch replace those of `active`"
    );
    let (composed, launched) = launch_of_robots(PlacementSpec::default(), words, joins);
    let planned: Vec<PlannedDeployment> = composed
        .launcher
        .deployments
        .iter()
        .map(|deployment| {
            let DeploymentSource::Node { name, .. } = &deployment.source else {
                panic!("a copy of `robots` runs nodes");
            };
            let instances: Vec<(&str, Option<&str>)> = deployment
                .instances
                .iter()
                .map(|instance| (instance.instance_id.as_str(), None))
                .collect();
            planned_deployment(name, &instances)
        })
        .collect();
    let ordered: Vec<NodeKey> = planned
        .iter()
        .map(|item| NodeKey::new(&item.node_name, &item.node_tag))
        .collect();
    active.planned.extend(planned);
    active.placements = launched.placements;
    active.record_copies(composed.report.copies, &ordered, &launched.copy_core_nodes);
    for name in removed {
        active.copies.remove(&CopyName::new(*name).unwrap());
    }
    active
}
