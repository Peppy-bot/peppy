//! What this daemon remembers of the launch it coordinates: the launcher it
//! composed, the plan it resolved, where each instance runs, the copies on
//! the stack, and the clock and watchers the plan established. Every stack
//! change reads it, changes it, and writes it back.

use super::ChangeResult;
use super::action::StackChangeContext;
use super::launch::joinable::JoinableBuild;
use super::launch::watchers::LifecycleWatchers;
use super::launch::{NodeKey, PlannedDeployment};
use config::runtime::{CopyName, CoreNodeName};
use daemon_config::launcher::{
    ClockIncarnations, CopyRecord, PeppyLauncher, Placements, PreparedLauncher, ResolvedClocks,
    UnitSelection,
};
use std::collections::BTreeMap;

/// The launch this daemon coordinates: what it composed, where it runs,
/// and the copies on it.
#[derive(Debug, Clone)]
pub(crate) struct ActiveLaunch {
    pub(super) launch_id: String,
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
    pub(super) fn axis_selections(
        &self,
        placements: &Placements,
    ) -> Vec<core_node_api::encoding::AxisSelection> {
        self.record.axis_selections(placements, &self.core_node)
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
    pub(super) fn copies(&self) -> Vec<core_node_api::encoding::CopyInfo> {
        self.copies
            .values()
            .map(|copy| core_node_api::encoding::CopyInfo {
                name: copy.record.name.clone(),
                option: copy.record.option.clone(),
                core_node: copy.core_node.clone(),
                instance_ids: copy.record.instance_ids().cloned().collect(),
                selections: copy.axis_selections(&self.placements),
                set_members: copy.record.set_members.clone(),
            })
            .collect()
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
            prepared,
            flat,
            selection,
            placements,
            resolved: planned.clone(),
            planned,
            copies: BTreeMap::new(),
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
    /// on the coordinator when it names none.
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
            let name = record.name.clone();
            self.copies.insert(name, StackCopy { record, core_node });
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

    pub(super) fn check_name(&self, name: &CopyName) -> ChangeResult<()> {
        if self.copies.contains_key(name) {
            return Err(format!("copy `{name}` already exists; choose another name"));
        }
        Ok(())
    }
}

pub(super) fn active_launch(ctx: &StackChangeContext) -> ChangeResult<ActiveLaunch> {
    ctx.slice_ownership.active.lock().clone().ok_or_else(|| {
        "this daemon has no active launcher; run peppy stack launch LAUNCHER on the coordinator first".to_owned()
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
    use crate::services::stack::fixtures::planned_deployment;
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
}
