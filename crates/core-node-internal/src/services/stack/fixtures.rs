//! Planned deployments and placements the stack's unit tests share, so every
//! module tests against the one spelling of a pin, a node and a machine map.

use super::launch::PlannedDeployment;
use config::AnyType;
use config::runtime::{CoreNodeName, Name};
use daemon_config::launcher::{Deployment, DeploymentInstance, DeploymentSource, Placements};
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
