//! What this daemon remembers of the launch it coordinates: the launcher it
//! composed, the plan it resolved, where each instance runs, the copies on
//! the stack, the copies the launch started, and the clock and watchers the
//! plan established. Every stack change reads it, changes it, and writes it
//! back.

use super::ChangeResult;
use super::action::StackChangeContext;
use super::fleet_listing::launch_listing;
use super::launch::joinable::JoinableBuild;
use super::launch::watchers::LifecycleWatchers;
use super::launch::{NodeKey, PlannedDeployment};
use crate::services::federation::SliceOwnership;
use crate::services::fleet::{Process, RobotProcesses};
use config::runtime::{CopyName, CoreNodeName};
use core_node_api::encoding::{AxisSelection, CopyInfo};
use daemon_config::daemon_interface::fleet::{ChangeKind, FleetListing};
use daemon_config::launcher::{
    ClockIncarnations, CopyRecord, DeploymentInstance, PeppyLauncher, Placements, PreparedLauncher,
    ResolvedClocks, UnitSelection,
};
use node_stack::NodeStack;
use std::collections::BTreeMap;

/// The launch this daemon coordinates: what it composed, where it runs,
/// the copies on it, and the copies it started.
#[derive(Debug, Clone)]
pub(crate) struct ActiveLaunch {
    pub(super) launch_id: String,
    /// The launcher the launch ran, as the listings name it.
    pub(super) launcher: String,
    pub(super) prepared: PreparedLauncher,
    pub(super) flat: PeppyLauncher,
    /// The stack's selection: the launcher's own axes and those of the
    /// options they selected, which every join composes against.
    pub(super) selection: UnitSelection,
    pub(super) placements: Placements,
    /// The deployments as they run: the instances of every source with at
    /// least one.
    pub(super) planned: Vec<PlannedDeployment>,
    /// Every source this launch resolved, as it resolved it: its own
    /// deployments, its joinable nodes and the sources its joins brought. A
    /// copy that runs one of these sources runs the launch's node, with its
    /// pins, however many joins and removals came between: a join of a
    /// scoped option stages the trees the launch built.
    pub(super) resolved: Vec<PlannedDeployment>,
    pub(super) copies: BTreeMap<CopyName, StackCopy>,
    /// The copies the launch started, each as the launch started it, with
    /// its machine and the machine of each part as the launch placed them.
    /// Joins and removals change `copies` and `placements` and leave these
    /// as they are, so a listing names them until the record goes.
    pub(super) launch_copies: BTreeMap<CopyName, CopyInfo>,
    /// The lifetime minted for each simulated domain this launch declares.
    /// A join reads them so it follows the domains already running instead of
    /// replacing the timelines under their consumers.
    pub(super) clocks: ClockIncarnations,
    /// The clock every instance of the plan reads, as this launch resolved
    /// it: what each instance was started with, and what a join holds its new
    /// connections against.
    pub(super) resolved_clocks: ResolvedClocks,
    /// The machines watching each source of this plan, as every machine of
    /// the launch was told them. A failed join puts these back.
    pub(super) watchers: LifecycleWatchers,
}

/// The daemon targets of one instance of a launch, each with the interface
/// it names and what the launch gave it (a scope or a vacancy, as written),
/// and whether the instance is a built-in MCP server.
#[derive(Debug, Clone)]
pub(crate) struct InstanceTargets {
    pub(crate) mcp_server: bool,
    pub(crate) targets: Vec<(
        daemon_config::daemon_interface::DaemonInterface,
        serde_json::Value,
    )>,
}

/// One copy on the stack: the record the composer keeps of it, and the
/// copy's machine, the machine its name places, which runs its base. Where
/// each of its instances runs is in the launch's placements.
#[derive(Debug, Clone)]
pub(super) struct StackCopy {
    pub(super) record: CopyRecord,
    pub(super) core_node: CoreNodeName,
}

impl StackCopy {
    /// The copy's filled own axes, each with the machine of its part when
    /// `placements` run that part away from the copy's machine.
    pub(super) fn axis_selections(&self, placements: &Placements) -> Vec<AxisSelection> {
        self.record.axis_selections(placements, &self.core_node)
    }

    /// The copy as a listing gives it, with the machine of each part that
    /// `placements` run away from the copy's machine.
    fn info(&self, placements: &Placements) -> CopyInfo {
        CopyInfo {
            name: self.record.name.clone(),
            option: self.record.option.clone(),
            core_node: self.core_node.clone(),
            instance_ids: self.record.instance_ids().cloned().collect(),
            selections: self.axis_selections(placements),
            set_members: self.record.set_members.clone(),
        }
    }

    /// The selections as a listing and a join's line print them: the words
    /// that add the copy again, joined by `,`.
    pub(super) fn words(&self, placements: &Placements) -> String {
        self.axis_selections(placements)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }
}

impl ActiveLaunch {
    /// The copies on the stack as a listing gives them, in name order.
    pub(crate) fn copies(&self) -> Vec<CopyInfo> {
        self.copies
            .values()
            .map(|copy| copy.info(&self.placements))
            .collect()
    }

    /// The copies the launch started, each as the launch started it, in
    /// name order.
    pub(crate) fn launch_copies(&self) -> Vec<CopyInfo> {
        self.launch_copies.values().cloned().collect()
    }

    pub(super) fn new(
        launch_id: &str,
        prepared: PreparedLauncher,
        flat: PeppyLauncher,
        selection: UnitSelection,
        placements: Placements,
        planned: Vec<PlannedDeployment>,
    ) -> Self {
        Self {
            launch_id: launch_id.to_owned(),
            launcher: String::new(),
            prepared,
            flat,
            selection,
            placements,
            resolved: planned.clone(),
            planned,
            copies: BTreeMap::new(),
            launch_copies: BTreeMap::new(),
            clocks: ClockIncarnations::new(),
            resolved_clocks: ResolvedClocks::default(),
            watchers: LifecycleWatchers::new(),
        }
    }

    /// Records what the change resolved: the clock each instance reads, and
    /// the lifetime each simulated domain was minted for.
    pub(super) fn with_clocks(
        mut self,
        resolved: ResolvedClocks,
        incarnations: ClockIncarnations,
    ) -> Self {
        self.resolved_clocks = resolved;
        self.clocks = incarnations;
        self
    }

    /// Records the launcher the launch ran, as the listings name it.
    pub(super) fn with_launcher(mut self, launcher: String) -> Self {
        self.launcher = launcher;
        self
    }

    pub(super) fn with_watchers(mut self, watchers: LifecycleWatchers) -> Self {
        self.watchers = watchers;
        self
    }

    /// Records the resolution of each joinable node the launch built, beside
    /// the resolutions of its own deployments.
    pub(super) fn with_joinable_resolutions(mut self, builds: &[JoinableBuild]) -> Self {
        for build in builds {
            let source = &build.planned.deployment.source;
            if !self
                .resolved
                .iter()
                .any(|item| &item.deployment.source == source)
            {
                self.resolved.push(build.planned.clone());
            }
        }
        self
    }

    /// Records the copies a launch started, each one's instances in the
    /// order they start, on the copy's machine `copy_core_nodes` gives it, or
    /// on the coordinator when it names none, and keeps each as the launch
    /// started it, with the machines the launch's placements give its
    /// parts.
    pub(super) fn record_copies(
        &mut self,
        copies: Vec<CopyRecord>,
        ordered: &[NodeKey],
        copy_core_nodes: &BTreeMap<CopyName, CoreNodeName>,
    ) {
        for copy in copies {
            let core_node = copy_core_nodes
                .get(&copy.name)
                .cloned()
                .unwrap_or_else(|| self.placements.coordinator_name().clone());
            let record = copy_in_start_order(copy, &self.planned, ordered);
            let copy = StackCopy { record, core_node };
            let name = copy.record.name.clone();
            self.launch_copies
                .insert(name.clone(), copy.info(&self.placements));
            self.copies.insert(name, copy);
        }
    }

    /// The record of every copy on the stack.
    pub(super) fn copy_records(&self) -> impl Iterator<Item = &CopyRecord> {
        self.copies.values().map(|copy| &copy.record)
    }

    /// The option of the copy called `name`, when the stack holds one.
    pub(super) fn copy_option(&self, name: &CopyName) -> Option<&str> {
        self.copies
            .get(name)
            .map(|copy| copy.record.option.as_str())
    }

    /// The daemon targets of the instance `instance_id` of this launch, with
    /// what the launch gave each, and whether the instance is a built-in MCP
    /// server; `None` for an id that is no instance of this launch.
    pub(crate) fn daemon_targets_of(&self, instance_id: &str) -> Option<InstanceTargets> {
        let (planned, instance) = self.planned_instance(instance_id)?;
        Some(InstanceTargets {
            mcp_server: matches!(
                planned.root,
                daemon_config::repository::DeploymentRoot::Exposures(_)
            ),
            targets: planned
                .targets
                .daemon
                .iter()
                .filter_map(|(target, interface)| {
                    let scope = instance.daemon_scopes.get(target)?;
                    Some((*interface, scope.clone()))
                })
                .collect(),
        })
    }

    /// Whether `instance_id` is an instance of this launch.
    pub(crate) fn has_instance(&self, instance_id: &str) -> bool {
        self.planned_instance(instance_id).is_some()
    }

    /// The instance `instance_id` of this launch, with the deployment that
    /// plans it.
    fn planned_instance(
        &self,
        instance_id: &str,
    ) -> Option<(&PlannedDeployment, &DeploymentInstance)> {
        self.planned.iter().find_map(|planned| {
            planned
                .deployment
                .instances
                .iter()
                .find(|instance| instance.instance_id.as_str() == instance_id)
                .map(|instance| (planned, instance))
        })
    }

    /// The copy that the instance `instance_id` belongs to: the robot of the
    /// instance, or `None` for an instance outside any copy.
    pub(crate) fn robot_of(&self, instance_id: &str) -> Option<&CopyName> {
        self.copies.values().find_map(|copy| {
            copy.record
                .instances
                .iter()
                .any(|instance| instance.instance_id.as_str() == instance_id)
                .then_some(&copy.record.name)
        })
    }

    /// Each robot of this launch, with the process of each of its
    /// instances on its machine.
    pub(crate) fn robot_processes(&self) -> Vec<RobotProcesses> {
        self.copies
            .values()
            .map(|copy| RobotProcesses {
                robot: copy.record.name.as_str().to_owned(),
                processes: copy
                    .record
                    .instances
                    .iter()
                    .map(|instance| {
                        let id = instance.instance_id.as_str();
                        Process::new(self.placements.of(id), id)
                    })
                    .collect(),
            })
            .collect()
    }

    /// Whether the stack holds a copy called `name`.
    pub(crate) fn holds_copy(&self, name: &str) -> bool {
        self.copies.keys().any(|copy| copy.as_str() == name)
    }

    /// The machine of the instance `instance_id` of this launch, or `None`
    /// for an id that is no instance of this launch.
    pub(crate) fn machine_of(&self, instance_id: &str) -> Option<&str> {
        self.planned
            .iter()
            .flat_map(|planned| &planned.deployment.instances)
            .any(|instance| instance.instance_id.as_str() == instance_id)
            .then(|| self.placements.of(instance_id))
    }

    pub(super) fn check_name(&self, name: &CopyName) -> ChangeResult<()> {
        if self.copies.contains_key(name) {
            return Err(super::precondition_failed(format!(
                "copy `{name}` already exists; choose another name"
            ))
            .into());
        }
        Ok(())
    }
}

/// Drops the launch this daemon coordinates, for `cause`: a new launch, a
/// launch that failed or a stack reset. Every lease and offer ends with it,
/// and the followers of the fleet read the removal of each of its copies.
pub(crate) fn drop_active_launch(ownership: &SliceOwnership, cause: &str) {
    let mut record = ownership.active.lock();
    let removed: Vec<String> = record
        .take()
        .map(|active| {
            active
                .copies
                .keys()
                .map(|name| name.as_str().to_owned())
                .collect()
        })
        .unwrap_or_default();
    ownership.fleet.with(|book| {
        book.end_every_lease(cause);
        book.set_launch(FleetListing::default());
        for robot in &removed {
            book.note(ChangeKind::Removal, robot);
        }
        book.sync_software(&[]);
    });
}

/// Makes `active` the launch this daemon coordinates, from the commit point
/// of the launch on, while its instances still start: its instances call the
/// daemon within the scopes of the launch, and the followers of the fleet
/// read the join of each of its copies. `node_stack` gives the documents of
/// the listing.
pub(super) fn commit_active_launch(
    ownership: &SliceOwnership,
    node_stack: &NodeStack,
    active: ActiveLaunch,
) {
    let joined: Vec<String> = active
        .copies
        .keys()
        .map(|name| name.as_str().to_owned())
        .collect();
    let robots = active.robot_processes();
    let listing = launch_listing(&active, node_stack);
    let mut record = ownership.active.lock();
    *record = Some(active);
    ownership.fleet.with(|book| {
        book.set_launch(listing);
        for robot in &joined {
            book.note(ChangeKind::Join, robot);
        }
        book.sync_software(&robots);
    });
}

/// Reads the part of the fleet listing that the record of the launch gives
/// again, once the nodes of the launch hold their documents, with no change
/// of the fleet: the record did not change.
pub(super) fn refresh_launch_listing(ownership: &SliceOwnership, node_stack: &NodeStack) {
    let Some(active) = ownership.active.lock().clone() else {
        return;
    };
    let listing = launch_listing(&active, node_stack);
    let record = ownership.active.lock();
    if record
        .as_ref()
        .is_some_and(|current| current.launch_id == active.launch_id)
    {
        ownership.fleet.with(|book| book.set_launch(listing));
    }
}

pub(super) fn active_launch(ctx: &StackChangeContext) -> ChangeResult<ActiveLaunch> {
    ctx.slice_ownership.active.lock().clone().ok_or_else(|| {
        super::precondition_failed(
            "this daemon has no active launcher; run peppy stack launch LAUNCHER on the \
             coordinator first",
        )
        .into()
    })
}

/// `copy` with its own instances of `planned` in the order their nodes start.
/// An instance whose node `ordered` does not name has no place to start, and
/// the record leaves it out.
pub(super) fn copy_in_start_order(
    copy: CopyRecord,
    planned: &[PlannedDeployment],
    ordered: &[NodeKey],
) -> CopyRecord {
    let instances = ordered
        .iter()
        .flat_map(|key| {
            planned
                .iter()
                .filter(move |item| item.node_name == key.name && item.node_tag == key.tag)
        })
        .flat_map(|item| &item.deployment.instances)
        .filter_map(|instance| {
            copy.instances
                .iter()
                .find(|owned| owned.instance_id == instance.instance_id)
                .cloned()
        })
        .collect();
    CopyRecord { instances, ..copy }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::stack::fixtures::{placements_with, planned_deployment, robots};
    use daemon_config::launcher::CopyInstance;

    /// A copy of `alpha` owning the instances its fragment wrote as
    /// `in_copy`, each minted as composition mints it.
    fn alpha(in_copy: &[&str]) -> CopyRecord {
        let name = CopyName::new("alpha").unwrap();
        CopyRecord {
            instances: in_copy
                .iter()
                .map(|id| CopyInstance {
                    instance_id: config::runtime::instance_id_in_copy(name.as_name(), id),
                    in_copy: config::runtime::Name::new(*id).unwrap(),
                    own_axis: None,
                })
                .collect(),
            name,
            axis: "robot".into(),
            option: "real".into(),
            selection: Default::default(),
            set_members: Vec::new(),
        }
    }

    /// A copy's instances are recorded in the order the dependency order
    /// starts their nodes, whatever order the plan lists them in, each
    /// keeping the id its fragment wrote, and every instance another copy
    /// owns is left out.
    #[test]
    fn a_copy_records_its_own_instances_in_the_order_their_nodes_start() {
        let planned = vec![
            planned_deployment("recorder", &[("alpha_recorder", None)]),
            planned_deployment(
                "arm",
                &[
                    ("alpha_arm", None),
                    ("bravo_arm", None),
                    ("alpha_spare_arm", None),
                ],
            ),
        ];
        let ordered = [NodeKey::new("arm", "v1"), NodeKey::new("recorder", "v1")];
        let copy = alpha(&["arm", "recorder", "spare_arm"]);

        let recorded = |copy: CopyRecord, ordered: &[NodeKey]| {
            copy_in_start_order(copy, &planned, ordered)
                .instances
                .iter()
                .map(|owned| {
                    (
                        owned.instance_id.as_str().to_owned(),
                        owned.in_copy.as_str().to_owned(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            recorded(copy.clone(), &ordered),
            [
                ("alpha_arm".to_owned(), "arm".to_owned()),
                ("alpha_spare_arm".to_owned(), "spare_arm".to_owned()),
                ("alpha_recorder".to_owned(), "recorder".to_owned()),
            ]
        );
        assert!(
            recorded(alpha(&[]), &ordered).is_empty(),
            "a copy owning nothing records nothing"
        );
        assert!(
            recorded(copy, &[]).is_empty(),
            "an instance whose node is unordered has no place to start"
        );
    }

    /// The launch copies are the copies as the launch recorded them, the
    /// file's copy and the `--join` one, each with its machine and the
    /// machine of each part as the launch's placements gave them. A removal
    /// and a join change the copies and the placements and leave the launch
    /// copies as they were.
    #[test]
    fn the_launch_copies_keep_the_copies_as_the_launch_recorded_them() {
        let words = ["alpha.pad@cn-gpu-box".parse().unwrap()];
        let joins = ["arm:alpha@cn-jetson-1".parse().unwrap()];
        let composed = robots()
            .launch(&words, &joins)
            .expect("the launch composes");
        let on_jetson_2 = [
            ("bravo_arm_inst", "cn-jetson-2"),
            ("bravo_commander_inst", "cn-jetson-2"),
        ];
        let placements = placements_with(
            "cn-desk",
            &[
                &on_jetson_2[..],
                &[
                    ("alpha_arm_inst", "cn-jetson-1"),
                    ("alpha_commander_inst", "cn-gpu-box"),
                ],
            ]
            .concat(),
        );
        let planned = vec![
            planned_deployment("arm", &[("bravo_arm_inst", None), ("alpha_arm_inst", None)]),
            planned_deployment(
                "pad",
                &[
                    ("alpha_commander_inst", None),
                    ("bravo_commander_inst", None),
                ],
            ),
        ];
        let ordered = [NodeKey::new("arm", "v1"), NodeKey::new("pad", "v1")];
        let copy_core_nodes = BTreeMap::from([
            (
                CopyName::new("alpha").unwrap(),
                CoreNodeName::new("cn-jetson-1").unwrap(),
            ),
            (
                CopyName::new("bravo").unwrap(),
                CoreNodeName::new("cn-jetson-2").unwrap(),
            ),
        ]);
        let mut active = ActiveLaunch::new(
            "launch-1",
            robots(),
            composed.launcher,
            composed.selection,
            placements,
            planned,
        );
        active.record_copies(composed.report.copies, &ordered, &copy_core_nodes);

        let launched = |name: &str, machine: &str, part: &str| CopyInfo {
            name: CopyName::new(name).unwrap(),
            option: "arm".to_owned(),
            core_node: CoreNodeName::new(machine).unwrap(),
            instance_ids: vec![
                config::runtime::Name::new(format!("{name}_arm_inst")).unwrap(),
                config::runtime::Name::new(format!("{name}_commander_inst")).unwrap(),
            ],
            selections: vec![part.parse().unwrap()],
            set_members: Vec::new(),
        };
        let as_launched = BTreeMap::from([
            (
                CopyName::new("alpha").unwrap(),
                launched("alpha", "cn-jetson-1", "commander=pad@cn-gpu-box"),
            ),
            (
                CopyName::new("bravo").unwrap(),
                launched("bravo", "cn-jetson-2", "commander=pad"),
            ),
        ]);
        assert_eq!(
            active.launch_copies, as_launched,
            "the file's copy and the --join one, each part on its machine"
        );
        assert_eq!(
            active.copies(),
            as_launched.values().cloned().collect::<Vec<_>>(),
            "the copies on the stack are the launch's"
        );

        // A removal of alpha, then a join: the join rebuilds the placements
        // from the instances that run.
        active.copies.remove(&CopyName::new("alpha").unwrap());
        active.placements = placements_with("cn-desk", &on_jetson_2);
        assert_eq!(
            active.copies(),
            vec![as_launched[&CopyName::new("bravo").unwrap()].clone()]
        );
        assert_eq!(
            active.launch_copies, as_launched,
            "a removal and a join leave the launch copies as the launch recorded them"
        );
    }
}
