//! The fleet listing of `fleet:v1`: each robot of the launch this daemon
//! coordinates with its kind, its clock, its software, its robot commander,
//! its instances and its contracts; the leases and the halt; the contracts of
//! each option and feature that a `stack_copies` scope of the launch names,
//! with their pinned documents; and each node with the manifest and the
//! commit that each machine runs.
//!
//! The listing reads the record of the launch and the book of the fleet in
//! one step ([`fleet_listing`]), so it gives the state that the last change
//! of the fleet left.

use super::state::ActiveLaunch;
use crate::services::federation::SliceOwnership;
use config::runtime::ClockBinding;
use daemon_config::consts::peppy_build;
use daemon_config::daemon_interface::fleet::{
    ContractPin, FleetDocument, FleetFeature, FleetInstance, FleetListing, FleetNode, FleetOption,
    FleetRobot, NodeInstance, NodeMachine, RobotContract, RobotKind, SoftwareState,
};
use daemon_config::daemon_interface::{DaemonInterface, DaemonScope, ScopedOption, TargetScope};
use daemon_config::launcher::Deployment;
use daemon_config::repository::{DeploymentRoot, PinKind};
use node_stack::NodeStack;
use std::collections::{BTreeMap, BTreeSet};

/// The release tag of this peppy, or `unknown` for a build that is no
/// release: a dev build, a release candidate, a build with no tag.
pub(crate) fn release_version() -> String {
    match peppy_build() {
        core_node_api::encoding::PeppyBuild::Release(version) => version,
        core_node_api::encoding::PeppyBuild::Unreleased => "unknown".to_owned(),
    }
}

/// The fleet listing of the launch this daemon, `core_node`, coordinates.
pub(crate) fn fleet_listing(
    ownership: &SliceOwnership,
    node_stack: &NodeStack,
    core_node: &str,
) -> FleetListing {
    let active = ownership.active.lock();
    let book = ownership.fleet.with(|book| book.view());
    let mut listing = FleetListing {
        version: release_version(),
        core_node: core_node.to_owned(),
        halted: book.halted_by.is_some(),
        halted_by: book.halted_by.unwrap_or_default(),
        leases: book.leases,
        ..FleetListing::default()
    };
    let Some(active) = active.as_ref() else {
        return listing;
    };
    listing.launcher = active.launcher.clone();
    listing.robots = active
        .copies
        .values()
        .map(|copy| {
            let mut robot = robot_entry(active, copy);
            if let Some(software) = book.software.get(&robot.name) {
                robot.software = *software;
            }
            robot
        })
        .collect();
    listing.options = scoped_options(active);
    listing.documents = documents_of(&listing.options, node_stack);
    listing.nodes = nodes_of(active);
    listing
}

/// One robot of the listing, its software state still to read.
fn robot_entry(active: &ActiveLaunch, copy: &super::state::StackCopy) -> FleetRobot {
    let record = &copy.record;
    let mut listed = Vec::new();
    let mut contracts: BTreeMap<(String, String, String), Vec<String>> = BTreeMap::new();
    let mut clocks: BTreeSet<String> = BTreeSet::new();
    let mut every_simulated = true;
    for owned in &record.instances {
        let id = owned.instance_id.as_str();
        let machine = active.placements.of(id).to_owned();
        let planned = active.planned.iter().find(|planned| {
            planned
                .deployment
                .instances
                .iter()
                .any(|i| i.instance_id.as_str() == id)
        });
        let (node, node_tag) = planned
            .map(|planned| (planned.node_name.clone(), planned.node_tag.clone()))
            .unwrap_or_default();
        if let Some(planned) = planned {
            for (name, tag, sha256) in implemented_contracts(planned) {
                contracts
                    .entry((name, tag, sha256))
                    .or_default()
                    .push(owned.in_copy.as_str().to_owned());
            }
        }
        match active.resolved_clocks.of(id) {
            ClockBinding::Wall => {
                every_simulated = false;
                clocks.insert("wall".to_owned());
            }
            binding => {
                if let Some(domain) = binding.domain() {
                    clocks.insert(domain.to_string());
                }
            }
        }
        listed.push(FleetInstance {
            name: owned.in_copy.as_str().to_owned(),
            instance: id.to_owned(),
            core_node: machine,
            node,
            node_tag,
        });
    }
    let clock = if clocks.len() == 1 {
        clocks.into_iter().next().unwrap_or_default()
    } else if clocks.contains("wall") {
        "wall".to_owned()
    } else {
        clocks.into_iter().collect::<Vec<_>>().join(",")
    };
    let robot_commander = active
        .prepared
        .robot_commander_axis_of(&record.option)
        .and_then(|axis| {
            record
                .selection
                .entries
                .iter()
                .find(|entry| entry.axis == axis)
                .and_then(|entry| entry.option.clone())
        })
        .filter(|option| option != "none")
        .unwrap_or_default();
    FleetRobot {
        name: record.name.as_str().to_owned(),
        option: record.option.clone(),
        core_node: copy.core_node.as_str().to_owned(),
        kind: if every_simulated && !record.instances.is_empty() {
            RobotKind::Simulated
        } else {
            RobotKind::Physical
        },
        clock,
        software: SoftwareState::Stopped,
        robot_commander,
        instances: listed,
        contracts: contracts
            .into_iter()
            .map(|((name, tag, sha256), instances)| RobotContract {
                name,
                tag,
                sha256,
                instances,
            })
            .collect(),
    }
}

/// The contracts that the node of `planned` implements, each with the
/// sha256 of the document the launch pinned for it.
fn implemented_contracts(
    planned: &super::launch::PlannedDeployment,
) -> Vec<(String, String, String)> {
    planned
        .config
        .manifest
        .implements
        .iter()
        .map(|entry| {
            let sha256 = planned
                .closure_pins
                .iter()
                .find(|pin| {
                    pin.kind == PinKind::Contract
                        && pin.name.as_str() == entry.name.as_str()
                        && pin.tag.as_str() == entry.tag
                })
                .map(|pin| pin.sha256.to_string())
                .or_else(|| entry.sha256.clone())
                .unwrap_or_default();
            (entry.name.as_str().to_owned(), entry.tag.clone(), sha256)
        })
        .collect()
}

/// The options of every `stack_copies` scope that the launch gives, each
/// once, in the order the scopes list them.
pub(crate) fn scopes_of_launch(active: &ActiveLaunch) -> Vec<ScopedOption> {
    let mut options: Vec<ScopedOption> = Vec::new();
    for planned in &active.planned {
        for (target, interface) in &planned.targets.daemon {
            if *interface != DaemonInterface::StackCopies {
                continue;
            }
            for instance in &planned.deployment.instances {
                let Some(value) = instance.daemon_scopes.get(target) else {
                    continue;
                };
                let Ok(TargetScope::Scoped(DaemonScope::StackCopies(scope))) =
                    interface.parse_target_scope(value)
                else {
                    continue;
                };
                for option in scope.options() {
                    if !options.iter().any(|held| held.option() == option.option()) {
                        options.push(option.clone());
                    }
                }
            }
        }
    }
    options
}

/// The contracts and the kind of each option of a `stack_copies` scope of
/// the launch, with the contracts that each of its features adds.
fn scoped_options(active: &ActiveLaunch) -> Vec<FleetOption> {
    let Ok(trials) = active.prepared.trial_stack(&active.selection) else {
        return Vec::new();
    };
    scopes_of_launch(active)
        .iter()
        .filter_map(|entry| {
            let base = trials.join(entry.option(), &[]).ok()?;
            let base_contracts = contracts_of_deployments(active, &base.deployments);
            let every_feature: Vec<_> = entry
                .features()
                .iter()
                .map(|feature| feature.word(None))
                .collect();
            let kind = trials
                .join(entry.option(), &every_feature)
                .map(|trial| kind_of_deployments(&trial.deployments))
                .unwrap_or(RobotKind::Physical);
            let features = entry
                .features()
                .iter()
                .map(|feature| {
                    let with = trials
                        .join(entry.option(), &[feature.word(None)])
                        .map(|trial| contracts_of_deployments(active, &trial.deployments))
                        .unwrap_or_default();
                    FleetFeature {
                        feature: feature.feature().as_str().to_owned(),
                        contracts: with.difference(&base_contracts).cloned().collect(),
                    }
                })
                .collect();
            Some(FleetOption {
                option: entry.option().as_str().to_owned(),
                kind,
                contracts: base_contracts.into_iter().collect(),
                features,
            })
        })
        .collect()
}

/// Simulated when every instance of `deployments` reads the clock of a
/// simulation: a clock that names a domain, not wall time.
pub(crate) fn kind_of_deployments(deployments: &[Deployment]) -> RobotKind {
    let mut instances = deployments
        .iter()
        .flat_map(|deployment| &deployment.instances)
        .peekable();
    if instances.peek().is_none() {
        return RobotKind::Physical;
    }
    let every_simulated = instances.all(|instance| {
        instance
            .framework
            .clock
            .as_ref()
            .is_some_and(|clock| clock.as_str() != daemon_config::launcher::WALL_CLOCK)
    });
    if every_simulated {
        RobotKind::Simulated
    } else {
        RobotKind::Physical
    }
}

/// The contracts that the nodes of `deployments` implement, with the sha256
/// of the documents that the launch resolved for them.
fn contracts_of_deployments(
    active: &ActiveLaunch,
    deployments: &[Deployment],
) -> BTreeSet<ContractPin> {
    deployments
        .iter()
        .filter_map(|deployment| {
            active
                .resolved
                .iter()
                .find(|planned| planned.deployment.source == deployment.source)
        })
        .flat_map(implemented_contracts)
        .map(|(name, tag, sha256)| ContractPin { name, tag, sha256 })
        .collect()
}

/// The pinned document of each contract that `options` names, as an entity
/// of the stack keeps it.
fn documents_of(options: &[FleetOption], node_stack: &NodeStack) -> Vec<FleetDocument> {
    let wanted: BTreeSet<&ContractPin> = options
        .iter()
        .flat_map(|option| {
            option.contracts.iter().chain(
                option
                    .features
                    .iter()
                    .flat_map(|feature| &feature.contracts),
            )
        })
        .collect();
    wanted
        .into_iter()
        .filter_map(|pin| {
            pinned_document(node_stack, &pin.name, &pin.tag, &pin.sha256).map(|document| {
                FleetDocument {
                    name: pin.name.clone(),
                    tag: pin.tag.clone(),
                    sha256: pin.sha256.clone(),
                    document,
                }
            })
        })
        .collect()
}

/// The content of the contract document `name:tag` pinned at `sha256`, as
/// an entity of the stack keeps it.
pub(crate) fn pinned_document(
    node_stack: &NodeStack,
    name: &str,
    tag: &str,
    sha256: &str,
) -> Option<String> {
    node_stack.snapshot().iter().find_map(|entity| {
        let entity = entity.read();
        entity
            .documents()
            .iter()
            .find(|document| {
                document.pin.kind == PinKind::Contract
                    && document.pin.name.as_str() == name
                    && document.pin.tag.as_str() == tag
                    && document.pin.sha256 == sha256
            })
            .map(|document| document.content.clone())
    })
}

/// Each node of the launch, with the manifest and the commit that each
/// machine runs, and its instances.
fn nodes_of(active: &ActiveLaunch) -> Vec<FleetNode> {
    active
        .planned
        .iter()
        .map(|planned| {
            let commit = match &planned.root {
                DeploymentRoot::Node(pin) => pin
                    .origin
                    .commit()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                DeploymentRoot::Exposures(_) => String::new(),
            };
            let instances: Vec<NodeInstance> = planned
                .deployment
                .instances
                .iter()
                .map(|instance| {
                    let id = instance.instance_id.as_str();
                    NodeInstance {
                        instance: id.to_owned(),
                        core_node: active.placements.of(id).to_owned(),
                        robot: active
                            .robot_of(id)
                            .map(|copy| copy.as_str().to_owned())
                            .unwrap_or_default(),
                    }
                })
                .collect();
            let machines: BTreeSet<&str> = instances
                .iter()
                .map(|instance| instance.core_node.as_str())
                .collect();
            FleetNode {
                name: planned.node_name.clone(),
                tag: planned.node_tag.clone(),
                machines: machines
                    .into_iter()
                    .map(|machine| NodeMachine {
                        core_node: machine.to_owned(),
                        manifest_sha256: planned.config_sha256.clone(),
                        commit: commit.clone(),
                    })
                    .collect(),
                instances,
            }
        })
        .collect()
}
