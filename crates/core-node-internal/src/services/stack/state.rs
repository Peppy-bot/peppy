//! What this daemon remembers of the launch it coordinates: the launcher it
//! composed, the plan it resolved, where each instance runs, the copies on
//! the stack, and the clock and watchers the plan established. Every stack
//! change reads it, changes it, and writes it back.

use super::ChangeResult;
use super::action::StackChangeContext;
use super::launch::clock::TimeSource;
use super::launch::watchers::LifecycleWatchers;
use super::launch::{NodeKey, PlannedDeployment};
use config::runtime::{CoreNodeName, Name};
use daemon_config::launcher::{
    CopyRecord, PeppyLauncher, Placements, PreparedLauncher, UnitSelection,
};
use std::collections::{BTreeMap, HashSet};

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
    /// Every source this launch resolved, as it resolved it: a copy joining
    /// after its source's last instance left runs the launch's node, with
    /// its pins.
    pub(super) resolved: Vec<PlannedDeployment>,
    pub(super) copies: BTreeMap<Name, StackCopy>,
    pub(super) time_source: Option<TimeSource>,
    pub(super) clock: Option<super::launch::federated::ClockDemand>,
    /// The machines watching each source of this plan, as every machine of
    /// the launch was told them. A failed join puts these back.
    pub(super) watchers: LifecycleWatchers,
}

/// One copy on the stack: the record the composer keeps of it, and the
/// machine its instances run on.
#[derive(Debug, Clone)]
pub(super) struct StackCopy {
    pub(super) record: CopyRecord,
    pub(super) core_node: CoreNodeName,
}

impl ActiveLaunch {
    pub(super) fn copies(&self) -> Vec<core_node_api::encoding::CopyInfo> {
        self.copies
            .values()
            .map(|copy| core_node_api::encoding::CopyInfo {
                name: copy.record.name.clone(),
                option: copy.record.option.clone(),
                core_node: copy.core_node.clone(),
                instance_ids: copy.record.instance_ids.clone(),
                selections: copy
                    .record
                    .selection
                    .own_axes(&copy.record.axis)
                    .filter_map(|entry| {
                        entry
                            .option
                            .as_ref()
                            .map(|option| format!("{}={option}", entry.axis))
                    })
                    .collect(),
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
            time_source: None,
            clock: None,
            watchers: LifecycleWatchers::new(),
        }
    }

    pub(super) fn with_time_source(mut self, source: Option<TimeSource>) -> Self {
        self.time_source = source;
        self
    }

    pub(super) fn with_clock(mut self, clock: super::launch::federated::ClockDemand) -> Self {
        self.clock = Some(clock);
        self
    }

    pub(super) fn with_watchers(mut self, watchers: LifecycleWatchers) -> Self {
        self.watchers = watchers;
        self
    }

    /// Records the copies a launch started, each one's instances in the
    /// order they start.
    pub(super) fn record_copies(&mut self, copies: Vec<CopyRecord>, ordered: &[NodeKey]) {
        for copy in copies {
            let owned: HashSet<_> = copy.instance_ids.iter().collect();
            let instance_ids = instance_ids_in_start_order(&self.planned, ordered, &owned);
            let first = instance_ids.first().expect("a copy starts an instance");
            let core_node = self.placements.core_node_of(first.as_str()).clone();
            let name = copy.name.clone();
            self.copies.insert(
                name,
                StackCopy {
                    record: CopyRecord {
                        instance_ids,
                        ..copy
                    },
                    core_node,
                },
            );
        }
    }

    pub(super) fn check_name(&self, name: &Name) -> ChangeResult<()> {
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

/// The `owned` instances of `planned` in the order their nodes start.
pub(super) fn instance_ids_in_start_order(
    planned: &[PlannedDeployment],
    ordered: &[NodeKey],
    owned: &HashSet<&Name>,
) -> Vec<Name> {
    ordered
        .iter()
        .flat_map(|key| {
            planned
                .iter()
                .filter(move |item| key == &NodeKey::new(&item.node_name, &item.node_tag))
        })
        .flat_map(|item| &item.deployment.instances)
        .filter(|instance| owned.contains(&instance.instance_id))
        .map(|instance| instance.instance_id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::stack::fixtures::planned_deployment;

    /// A copy's instances are recorded in the order the dependency order
    /// starts their nodes, whatever order the plan lists them in, and every
    /// instance another copy owns is left out.
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
        let alpha = [
            Name::new("alpha_arm").unwrap(),
            Name::new("alpha_recorder").unwrap(),
            Name::new("alpha_spare_arm").unwrap(),
        ];
        let owned: HashSet<&Name> = alpha.iter().collect();

        assert_eq!(
            instance_ids_in_start_order(&planned, &ordered, &owned),
            [
                Name::new("alpha_arm").unwrap(),
                Name::new("alpha_spare_arm").unwrap(),
                Name::new("alpha_recorder").unwrap(),
            ]
        );
        assert!(
            instance_ids_in_start_order(&planned, &ordered, &HashSet::new()).is_empty(),
            "a copy owning nothing records nothing"
        );
        assert!(
            instance_ids_in_start_order(&planned, &[], &owned).is_empty(),
            "an instance whose node is unordered has no place to start"
        );
    }
}
