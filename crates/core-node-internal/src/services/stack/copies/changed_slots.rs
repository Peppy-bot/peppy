//! The set slots of stack instances whose members a copy changes: which ones
//! they are, checked against the manifests before the plan is validated, the
//! whole set each holds once it is, delivering those sets to the machines that
//! run them, and refusing a removal that would leave a `one_or_more` slot empty.

use super::super::STACK_QUERY_TIMEOUT;
use super::super::action::StackChangeContext;
use super::super::launch::PlannedDeployment;
use crate::services::node::binding::BINDING_UPDATE_TIMEOUT;
use crate::services::node::observation::OBSERVATION_UPDATE_TIMEOUT;
use config::node::Cardinality;
use config::runtime::{BoundProducers, Name, SlotBindings};
use core_node_api::encoding::{
    ObservationTargets, ParticipantSetsUpdateRequest, SetMember, SlotMembers, SlotSet,
};
use daemon_config::launcher::{CopyRecord, PeppyLauncher, Placements, PlannedObservation};
use futures::future::join_all;
use peppylib::core_node::transport::poll;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

/// Which kind of member set a slot holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetKind {
    Producers,
    Observed,
}

/// The change a copy's set slots are read for.
#[derive(Clone, Copy)]
pub(super) enum Change {
    /// A join grows each slot; a pairing slot is refused, since a copy pairs
    /// into one from its own instance's `links`.
    Join,
    /// A removal shrinks each slot; a pairing slot a launch-time copy added to
    /// loses its pair when the copy's instance stops, with nothing to deliver.
    Removal,
}

/// One set slot of a stack instance whose members a copy changes.
#[derive(Debug)]
pub(super) struct ChangedSlot {
    pub(super) instance_id: Name,
    link_id: String,
    /// The node the instance runs, `name:tag`, for refusals.
    node: String,
    kind: SetKind,
    cardinality: Cardinality,
}

impl ChangedSlot {
    /// This slot as a refusal names it.
    pub(super) fn field(&self) -> String {
        format!("`{}.links.{}`", self.instance_id, self.link_id)
    }

    /// The machine the slot's instance runs on.
    pub(super) fn host<'a>(&self, placements: &'a Placements) -> &'a str {
        placements.core_node_of(self.instance_id.as_str()).as_str()
    }
}

/// Whether `instance` holds one of `slots`.
pub(super) fn holds_any(slots: &[ChangedSlot], instance: &Name) -> bool {
    slots.iter().any(|slot| slot.instance_id == *instance)
}

/// The machines the instances of `slots` run on.
pub(super) fn hosts_of<'a>(
    slots: &'a [ChangedSlot],
    placements: &'a Placements,
) -> impl Iterator<Item = &'a str> {
    slots.iter().map(|slot| slot.host(placements))
}

/// The set slots `copy`'s members change, once each in the order it first adds
/// to them. Each must be a `one_or_more` or `zero_or_more` producer or observer
/// slot its instance's manifest declares; a refusal names the slot and the fix.
pub(super) fn changed_slots(
    copy: &CopyRecord,
    planned: &[PlannedDeployment],
    change: Change,
) -> Result<Vec<ChangedSlot>, String> {
    let mut slots: Vec<ChangedSlot> = Vec::new();
    for member in &copy.set_members {
        let seen = slots
            .iter()
            .any(|slot| slot.instance_id == member.instance_id && slot.link_id == member.link_id);
        if seen {
            continue;
        }
        if let Some(slot) = changed_slot(copy, planned, member, change)? {
            slots.push(slot);
        }
    }
    Ok(slots)
}

fn changed_slot(
    copy: &CopyRecord,
    planned: &[PlannedDeployment],
    member: &SetMember,
    change: Change,
) -> Result<Option<ChangedSlot>, String> {
    let (instance, slot) = (&member.instance_id, member.link_id.as_str());
    let field = format!("`{instance}.links.{slot}`");
    let item = planned
        .iter()
        .find(|item| {
            item.deployment
                .instances
                .iter()
                .any(|running| running.instance_id == *instance)
        })
        .expect("a copy's members join instances of the plan it was composed against");
    let node = format!("{}:{}", item.node_name, item.node_tag);
    let depends_on = item.config.manifest.depends_on.as_ref();
    let producer_slots = depends_on.into_iter().flat_map(|deps| {
        deps.nodes
            .iter()
            .map(|dep| (dep.link_id.as_str(), dep.cardinality))
            .chain(
                deps.contracts
                    .iter()
                    .map(|dep| (dep.link_id.as_str(), dep.cardinality)),
            )
            .map(|(link_id, cardinality)| (link_id, SetKind::Producers, cardinality))
    });
    let observer_slots = depends_on.into_iter().flat_map(|deps| {
        deps.pairing_observers
            .iter()
            .map(|dep| (dep.link_id.as_str(), SetKind::Observed, dep.cardinality))
    });
    let declared: Vec<(&str, SetKind, Cardinality)> =
        producer_slots.chain(observer_slots).collect();
    let pairing = depends_on
        .into_iter()
        .flat_map(|deps| &deps.pairings)
        .any(|dep| dep.link_id == slot);
    let copy_name = &copy.name;
    match declared.iter().find(|(link_id, ..)| *link_id == slot) {
        Some((_, _, cardinality)) if cardinality.is_scalar() => Err(format!(
            "copy `{copy_name}` has a member in {field}, which `{node}` declares `{}`; \
             `add_links` appends only to a `one_or_more` or `zero_or_more` slot: declare it \
             `one_or_more` or `zero_or_more` in the node's `depends_on`, run `peppy node sync` \
             and launch again",
            cardinality.as_str()
        )),
        Some((_, kind, cardinality)) => Ok(Some(ChangedSlot {
            instance_id: instance.clone(),
            link_id: slot.to_string(),
            node,
            kind: *kind,
            cardinality: *cardinality,
        })),
        None if pairing => match change {
            Change::Join => Err(format!(
                "copy `{copy_name}` adds members to {field}, a pairing slot of `{node}`; a copy \
                 pairs into it from its own instance's `links`, naming `{instance}/{slot}`"
            )),
            Change::Removal => Ok(None),
        },
        None => {
            let set_slots: Vec<&str> = declared
                .iter()
                .filter(|(_, _, cardinality)| !cardinality.is_scalar())
                .map(|(link_id, ..)| *link_id)
                .collect();
            Err(if set_slots.is_empty() {
                format!(
                    "copy `{copy_name}` has a member in {field}, which `{node}` does not \
                     declare; declare a `one_or_more` or `zero_or_more` slot in `{node}`'s \
                     `depends_on`, run `peppy node sync` and launch again, or drop `{slot}` from \
                     the option's `add_links`"
                )
            } else {
                format!(
                    "copy `{copy_name}` has a member in {field}, which `{node}` does not \
                     declare; name one of its set slots in the option's `add_links`: {}",
                    daemon_config::format_quoted_list(set_slots)
                )
            })
        }
    }
}

/// Refuses a removal of `copy` that would leave a `one_or_more` slot of `slots`
/// with no member in `remaining`, naming every such slot and the fix.
pub(super) fn check_not_emptied(
    copy: &CopyRecord,
    slots: &[ChangedSlot],
    remaining: &PeppyLauncher,
) -> Result<(), String> {
    let emptied: Vec<&ChangedSlot> = slots
        .iter()
        .filter(|slot| slot.cardinality == Cardinality::OneOrMore)
        .filter(|slot| {
            remaining
                .deployments
                .iter()
                .flat_map(|deployment| &deployment.instances)
                .find(|running| running.instance_id == slot.instance_id)
                .and_then(|running| running.links.get(&slot.link_id))
                .and_then(|link| link.selection())
                .is_none_or(|selection| selection.targets().is_empty())
        })
        .collect();
    if emptied.is_empty() {
        return Ok(());
    }
    let fields = emptied
        .iter()
        .map(|slot| {
            format!(
                "{} (declared `one_or_more` by `{}`)",
                slot.field(),
                slot.node
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let slots = if emptied.len() == 1 {
        "that slot"
    } else {
        "those slots"
    };
    // A copy the launcher file deploys may add a stack instance to a set,
    // which a join refuses, so another copy of the option is not the way back.
    let adds_stack_instances = copy.set_members.iter().any(|member| {
        emptied
            .iter()
            .any(|slot| slot.instance_id == member.instance_id && slot.link_id == member.link_id)
            && !copy
                .instance_ids
                .iter()
                .any(|id| id.as_str() == member.target.split('/').next().unwrap_or_default())
    });
    Err(if adds_stack_instances {
        format!(
            "removing copy `{name}` would leave {fields} with no member, and the launcher \
             deploys `{name}` to add stack instances to {slots}; take the whole stack down with \
             `peppy stack reset` and launch again",
            name = copy.name,
        )
    } else {
        format!(
            "removing copy `{name}` would leave {fields} with no member; join a copy that adds to \
             {slots} first (`peppy stack join {option} -i NAME`), then remove `{name}`, or take \
             the whole stack down with `peppy stack reset`",
            name = copy.name,
            option = copy.option,
        )
    })
}

/// The whole set each of `slots` holds in the validated plan, in plan order.
pub(super) fn whole_sets(
    slots: &[ChangedSlot],
    bindings: &BTreeMap<String, SlotBindings>,
    observations: &[PlannedObservation],
) -> Vec<SlotSet> {
    slots
        .iter()
        .map(|slot| SlotSet {
            instance_id: slot.instance_id.clone(),
            link_id: slot.link_id.clone(),
            members: match slot.kind {
                // A set slot the plan binds nothing to holds the empty set.
                SetKind::Producers => SlotMembers::Producers(
                    bindings
                        .get(slot.instance_id.as_str())
                        .and_then(|slots| slots.get(&slot.link_id))
                        .cloned()
                        .unwrap_or_else(|| {
                            assert!(
                                slot.cardinality.allows_empty(),
                                "a validated plan binds every slot with a floor"
                            );
                            BoundProducers::default()
                        }),
                ),
                SetKind::Observed => SlotMembers::Observed(
                    ObservationTargets::new(
                        &slot.link_id,
                        observations
                            .iter()
                            .filter(|observation| {
                                observation.observer_instance_id == slot.instance_id.as_str()
                                    && observation.observer_link_id == slot.link_id
                            })
                            .map(PlannedObservation::target)
                            .collect(),
                    )
                    .expect("plan members are duplicate-free by validation"),
                ),
            },
        })
        .collect()
}

/// Each of `sets` without the members `copy_instances` run: the whole set it
/// held before a join added them, since a join adds only its own instances.
pub(super) fn without_members_of(sets: &[SlotSet], copy_instances: &[Name]) -> Vec<SlotSet> {
    let owned = |instance_id: &str| copy_instances.iter().any(|id| id.as_str() == instance_id);
    sets.iter()
        .map(|set| SlotSet {
            instance_id: set.instance_id.clone(),
            link_id: set.link_id.clone(),
            members: match &set.members {
                SlotMembers::Producers(producers) => SlotMembers::Producers(
                    BoundProducers::try_from(
                        producers
                            .iter()
                            .filter(|producer| !owned(&producer.instance_id))
                            .cloned()
                            .collect::<Vec<_>>(),
                    )
                    .expect("a subset of a duplicate-free set is duplicate-free"),
                ),
                SlotMembers::Observed(targets) => SlotMembers::Observed(
                    ObservationTargets::new(
                        &set.link_id,
                        targets
                            .as_slice()
                            .iter()
                            .filter(|target| !owned(&target.source.instance_id))
                            .cloned()
                            .collect(),
                    )
                    .expect("a subset of a duplicate-free set is duplicate-free"),
                ),
            },
        })
        .collect()
}

/// What heals a slot whose delivery failed, for each way it can fail.
pub(super) const UNDELIVERED_REMEDY: &str = "An instance that is not running holds no set and \
     takes the whole set when the stack is launched again; a running instance keeps the set it \
     holds until the next `peppy stack join` or `peppy stack remove` changing that slot delivers \
     it whole";

/// Delivers the sets a join grew, and, when a delivery fails, puts back what
/// those sets held before the copy: the grown set minus the copy's own
/// instances, since a join adds only those. Fails either way: a set an
/// instance never took fails the join, which rolls back.
pub(super) async fn grow_or_restore<D, F>(
    deliver: D,
    grown: Vec<SlotSet>,
    copy_instances: &[Name],
) -> Result<(), String>
where
    D: Fn(Vec<SlotSet>) -> F,
    F: Future<Output = Result<(), String>>,
{
    let Err(failure) = deliver(grown.clone()).await else {
        return Ok(());
    };
    match deliver(without_members_of(&grown, copy_instances)).await {
        Ok(()) => Err(format!(
            "{failure}\nThe sets that took the copy's members hold what they held before it"
        )),
        Err(unrestored) => Err(format!(
            "{failure}\nand restoring the sets they held failed: {unrestored}\n{UNDELIVERED_REMEDY}"
        )),
    }
}

/// Delivers each set to the machine that runs its instance, every machine at
/// once: this daemon's own instances directly, a participant's in one request
/// to it. Fails naming each set that did not reach its instance and why.
pub(super) async fn deliver_sets(
    ctx: &StackChangeContext,
    launch_id: &str,
    placements: &Placements,
    sets: Vec<SlotSet>,
) -> Result<(), String> {
    let mut by_host: BTreeMap<String, Vec<SlotSet>> = BTreeMap::new();
    for set in sets {
        by_host
            .entry(
                placements
                    .core_node_of(set.instance_id.as_str())
                    .to_string(),
            )
            .or_default()
            .push(set);
    }
    let failures: Vec<String> = join_all(by_host.into_iter().map(|(host, sets)| async move {
        let failures = if host == ctx.bound_core_node {
            ctx.relationships.replace_sets(sets).await
        } else {
            replace_on_participant(ctx, launch_id, &host, sets).await
        };
        failures
            .into_iter()
            .map(|failure| format!("on `{host}`: {failure}"))
            .collect::<Vec<_>>()
    }))
    .await
    .into_iter()
    .flatten()
    .collect();
    if failures.is_empty() {
        return Ok(());
    }
    Err(format!(
        "these sets did not reach their instances:\n  {}",
        failures.join("\n  ")
    ))
}

/// Asks `host` to replace the sets of the instances it runs, returning one line
/// per set it could not replace.
/// How long a participant may take to replace `sets`: it tells its observer
/// instances in turn, each within `OBSERVATION_UPDATE_TIMEOUT`, and its
/// producer sets at once within `BINDING_UPDATE_TIMEOUT`, and the request
/// itself travels within `STACK_QUERY_TIMEOUT`.
fn sets_update_budget(sets: &[SlotSet]) -> Duration {
    let observers = sets
        .iter()
        .filter(|set| matches!(set.members, SlotMembers::Observed(_)))
        .map(|set| &set.instance_id)
        .collect::<BTreeSet<_>>()
        .len();
    OBSERVATION_UPDATE_TIMEOUT
        .saturating_mul(observers.try_into().unwrap_or(u32::MAX))
        .saturating_add(BINDING_UPDATE_TIMEOUT)
        .saturating_add(STACK_QUERY_TIMEOUT)
}

async fn replace_on_participant(
    ctx: &StackChangeContext,
    launch_id: &str,
    host: &str,
    sets: Vec<SlotSet>,
) -> Vec<String> {
    let budget = sets_update_budget(&sets);
    let request = ParticipantSetsUpdateRequest {
        launch_id: launch_id.to_owned(),
        sets,
    };
    match poll(
        &request,
        &ctx.messenger,
        &ctx.bound_core_node,
        &ctx.core_instance_id,
        host,
        budget,
    )
    .await
    {
        Ok(verdict) => verdict
            .into_result()
            .err()
            .map(|reasons| reasons.lines().map(str::to_owned).collect())
            .unwrap_or_default(),
        Err(error) => vec![format!("cannot be reached: {error}")],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::stack::fixtures::planned_deployment;
    use config::runtime::ProducerRef;
    use core_node_api::encoding::ObservationTarget;
    use daemon_config::launcher::{PeppyLauncherParser, UnitSelection};

    /// A monitor with a `zero_or_more` node slot `robots`, a `zero_or_more`
    /// contract slot `camera_profiles`, a `one` node slot `lead_robot`, a
    /// `one_or_more` observer slot `fleet`, and a pairing slot `leader`.
    const MONITOR_MANIFEST: &str = r#"{
        peppy_schema: "node/v1",
        manifest: {
            name: "monitor",
            tag: "v1",
            depends_on: {
                nodes: [
                    { name: "robot", tag: "v1", link_id: "robots", cardinality: "zero_or_more" },
                    { name: "robot", tag: "v1", link_id: "lead_robot" }
                ],
                contracts: [
                    { name: "camera_profile", tag: "v1", link_id: "camera_profiles", cardinality: "zero_or_more" }
                ],
                pairing_observers: [
                    { name: "joint_link", tag: "v1", role: "follower", link_id: "fleet", cardinality: "one_or_more" }
                ],
                pairings: [{ name: "joint_link", tag: "v1", role: "leader", link_id: "leader", cardinality: "zero_or_one" }]
            }
        },
        execution: { language: "rust", parameters: {}, build_cmd: ["true"], run_cmd: ["true"] },
        interfaces: { topics: {
            emits: [{ link_id: "leader", name: "joint_setpoints" }],
            consumes: [
                { link_id: "robots", name: "heartbeat" },
                { link_id: "camera_profiles", name: "frames" },
                { link_id: "lead_robot", name: "heartbeat" },
                { link_id: "fleet", name: "joint_states" },
                { link_id: "leader", name: "joint_states" }
            ]
        } }
    }"#;

    fn monitor() -> PlannedDeployment {
        PlannedDeployment {
            config: config::node::NodeConfigParser::from_content(MONITOR_MANIFEST)
                .expect("the monitor manifest parses"),
            ..planned_deployment("monitor", &[("monitor_inst", None)])
        }
    }

    /// Copy `alpha` of option `real`, which added each `(slot, target)` to
    /// `monitor_inst`.
    fn copy_adding(members: &[(&str, &str)]) -> CopyRecord {
        CopyRecord {
            name: Name::new("alpha").unwrap(),
            axis: "robot".into(),
            option: "real".into(),
            selection: UnitSelection::default(),
            instance_ids: vec![Name::new("alpha_arm_inst").unwrap()],
            set_members: members
                .iter()
                .map(|(slot, target)| SetMember {
                    instance_id: Name::new("monitor_inst").unwrap(),
                    link_id: (*slot).into(),
                    target: (*target).into(),
                })
                .collect(),
        }
    }

    fn slots_of(members: &[(&str, &str)], change: Change) -> Result<Vec<ChangedSlot>, String> {
        changed_slots(&copy_adding(members), &[monitor()], change)
    }

    /// The camera rig's shape: one copy adds several members to a contract slot
    /// and to a node slot of one stack instance, and observes through a third.
    #[test]
    fn a_copys_changed_slots_are_named_once_each_in_the_order_it_first_adds() {
        let slots = slots_of(
            &[
                ("fleet", "alpha_arm_inst/controller"),
                ("camera_profiles", "alpha_wrist_left"),
                ("robots", "alpha_arm_inst"),
                ("camera_profiles", "alpha_chest"),
                ("fleet", "alpha_arm_inst/wrist"),
            ],
            Change::Join,
        )
        .unwrap();
        assert_eq!(
            slots
                .iter()
                .map(|slot| (slot.link_id.as_str(), slot.kind, slot.cardinality))
                .collect::<Vec<_>>(),
            [
                ("fleet", SetKind::Observed, Cardinality::OneOrMore),
                (
                    "camera_profiles",
                    SetKind::Producers,
                    Cardinality::ZeroOrMore
                ),
                ("robots", SetKind::Producers, Cardinality::ZeroOrMore),
            ]
        );
    }

    #[test]
    fn a_join_adding_to_a_scalar_slot_is_refused_with_the_manifest_fix() {
        let error = slots_of(&[("lead_robot", "alpha_arm_inst")], Change::Join).unwrap_err();
        assert_eq!(
            error,
            "copy `alpha` has a member in `monitor_inst.links.lead_robot`, which `monitor:v1` \
             declares `one`; `add_links` appends only to a `one_or_more` or `zero_or_more` slot: \
             declare it `one_or_more` or `zero_or_more` in the node's `depends_on`, run `peppy \
             node sync` and launch again"
        );
    }

    #[test]
    fn a_join_adding_to_a_pairing_slot_is_refused_and_a_removal_leaves_it_to_the_pair() {
        let error = slots_of(&[("leader", "alpha_arm_inst")], Change::Join).unwrap_err();
        assert_eq!(
            error,
            "copy `alpha` adds members to `monitor_inst.links.leader`, a pairing slot of \
             `monitor:v1`; a copy pairs into it from its own instance's `links`, naming \
             `monitor_inst/leader`"
        );
        let slots = slots_of(&[("leader", "alpha_arm_inst")], Change::Removal)
            .expect("a launch-time copy's pairing member leaves with its pair");
        assert!(slots.is_empty());
    }

    #[test]
    fn adding_to_an_undeclared_slot_is_refused_naming_the_set_slots_to_use() {
        let error = slots_of(&[("ghosts", "alpha_arm_inst")], Change::Join).unwrap_err();
        assert_eq!(
            error,
            "copy `alpha` has a member in `monitor_inst.links.ghosts`, which `monitor:v1` does \
             not declare; name one of its set slots in the option's `add_links`: `robots`, \
             `camera_profiles`, `fleet`"
        );
    }

    #[test]
    fn a_removal_emptying_one_or_more_sets_is_refused_naming_each_and_one_leaving_a_member_passes()
    {
        let copy = copy_adding(&[("fleet", "alpha_arm_inst/controller")]);
        let slots = changed_slots(&copy, &[monitor()], Change::Removal).unwrap();
        let remaining = |fleet: &str| {
            PeppyLauncherParser::from_content(&format!(
                r#"{{
                peppy_schema: "launcher/v1",
                deployments: [
                    {{ source: {{ name: "monitor", tag: "v1" }},
                      instances: [{{ instance_id: "monitor_inst", links: {{ fleet: {fleet} }} }}] }},
                    {{ source: {{ name: "robot", tag: "v1" }},
                      instances: [{{ instance_id: "bravo_arm_inst" }}] }}
                ]
            }}"#
            ))
            .expect("the remaining stack parses")
        };
        let error = check_not_emptied(&copy, &slots, &remaining("[]")).unwrap_err();
        assert_eq!(
            error,
            "removing copy `alpha` would leave `monitor_inst.links.fleet` (declared `one_or_more` \
             by `monitor:v1`) with no member; join a copy that adds to that slot first (`peppy \
             stack join real -i NAME`), then remove `alpha`, or take the whole stack down with \
             `peppy stack reset`"
        );
        check_not_emptied(
            &copy,
            &slots,
            &remaining(r#"["bravo_arm_inst/controller"]"#),
        )
        .expect("a set keeping a member may shrink");
    }

    /// A copy the launcher file deploys may put a stack instance into a
    /// `one_or_more` set; removing it cannot be healed by a join of the same
    /// option, so the refusal names the launch route.
    #[test]
    fn a_removal_emptying_a_set_a_launch_time_copy_filled_with_a_stack_instance_names_the_launch_route()
     {
        let copy = copy_adding(&[("fleet", "eye/controller")]);
        let slots = changed_slots(&copy, &[monitor()], Change::Removal).unwrap();
        let remaining = PeppyLauncherParser::from_content(
            r#"{
                peppy_schema: "launcher/v1",
                deployments: [
                    { source: { name: "monitor", tag: "v1" },
                      instances: [{ instance_id: "monitor_inst", links: { fleet: [] } }] },
                    { source: { name: "robot", tag: "v1" },
                      instances: [{ instance_id: "eye" }] }
                ]
            }"#,
        )
        .expect("the remaining stack parses");
        let error = check_not_emptied(&copy, &slots, &remaining).unwrap_err();
        assert_eq!(
            error,
            "removing copy `alpha` would leave `monitor_inst.links.fleet` (declared \
             `one_or_more` by `monitor:v1`) with no member, and the launcher deploys `alpha` to \
             add stack instances to that slot; take the whole stack down with `peppy stack \
             reset` and launch again"
        );
    }

    fn observation(source: &str) -> PlannedObservation {
        PlannedObservation {
            observer_instance_id: "monitor_inst".into(),
            observer_link_id: "fleet".into(),
            pairing_name: "joint_link".into(),
            pairing_tag: "v1".into(),
            observed_role: "follower".into(),
            source: ProducerRef::new("cn-robot", source),
            source_link_id: "controller".into(),
            peer: None,
        }
    }

    fn members(set: &SlotSet) -> Vec<String> {
        match &set.members {
            SlotMembers::Producers(producers) => producers
                .iter()
                .map(|producer| producer.instance_id.clone())
                .collect(),
            SlotMembers::Observed(targets) => targets
                .as_slice()
                .iter()
                .map(|target: &ObservationTarget| target.source.instance_id.clone())
                .collect(),
        }
    }

    #[test]
    fn each_changed_set_is_the_plans_whole_set_in_plan_order() {
        let slots = slots_of(
            &[
                ("robots", "alpha_arm_inst"),
                ("fleet", "alpha_arm_inst/controller"),
            ],
            Change::Join,
        )
        .unwrap();
        let bindings = BTreeMap::from([(
            "monitor_inst".to_string(),
            SlotBindings::from([(
                "robots".to_string(),
                BoundProducers::try_from(vec![
                    ProducerRef::new("cn-robot", "bravo_arm_inst"),
                    ProducerRef::new("cn-cloud", "alpha_arm_inst"),
                ])
                .unwrap(),
            )]),
        )]);
        let sets = whole_sets(
            &slots,
            &bindings,
            &[observation("bravo_arm_inst"), observation("alpha_arm_inst")],
        );
        assert_eq!(
            sets.iter()
                .map(|set| (set.link_id.as_str(), members(set)))
                .collect::<Vec<_>>(),
            [
                (
                    "robots",
                    vec!["bravo_arm_inst".to_string(), "alpha_arm_inst".to_string()]
                ),
                (
                    "fleet",
                    vec!["bravo_arm_inst".to_string(), "alpha_arm_inst".to_string()]
                ),
            ]
        );
        assert!(matches!(sets[0].members, SlotMembers::Producers(_)));
        assert!(matches!(sets[1].members, SlotMembers::Observed(_)));

        let before = without_members_of(&sets, &[Name::new("alpha_arm_inst").unwrap()]);
        assert_eq!(
            before
                .iter()
                .map(|set| (set.link_id.as_str(), members(set)))
                .collect::<Vec<_>>(),
            [
                ("robots", vec!["bravo_arm_inst".to_string()]),
                ("fleet", vec!["bravo_arm_inst".to_string()]),
            ],
            "the sets a join grew, without its members, are what they held before it"
        );
    }

    /// The three outcomes of delivering a join's grown sets, driven through
    /// [`grow_or_restore`] with the delivery under the test's control: what
    /// each attempt carries, and what the join is told.
    mod growing_or_restoring {
        use super::*;
        use std::sync::{Arc, Mutex};

        /// One set slot holding `members`, enough for the seam to carry.
        fn set_of(members: &[&str]) -> Vec<SlotSet> {
            vec![SlotSet {
                instance_id: Name::new("monitor_inst").unwrap(),
                link_id: "robots".to_string(),
                members: SlotMembers::Producers(
                    BoundProducers::try_from(
                        members
                            .iter()
                            .map(|id| ProducerRef::new("cn-robot", *id))
                            .collect::<Vec<_>>(),
                    )
                    .unwrap(),
                ),
            }]
        }

        /// The member ids each delivery carried, in the order the seam
        /// delivered them.
        type Carried = Arc<Mutex<Vec<Vec<String>>>>;

        /// Records what each delivery carried and answers from `outcomes`.
        fn recording(
            outcomes: Vec<Result<(), String>>,
        ) -> (
            impl Fn(Vec<SlotSet>) -> std::future::Ready<Result<(), String>>,
            Carried,
        ) {
            let carried: Carried = Arc::new(Mutex::new(Vec::new()));
            let answers = Mutex::new(outcomes.into_iter());
            let recorded = Arc::clone(&carried);
            let deliver = move |sets: Vec<SlotSet>| {
                recorded
                    .lock()
                    .unwrap()
                    .push(sets.iter().flat_map(members).collect::<Vec<String>>());
                std::future::ready(
                    answers
                        .lock()
                        .unwrap()
                        .next()
                        .expect("the seam delivers no more than twice"),
                )
            };
            (deliver, carried)
        }

        #[tokio::test]
        async fn a_delivery_that_lands_leaves_the_grown_set_standing() {
            let (deliver, carried) = recording(vec![Ok(())]);
            let alpha = [Name::new("alpha_arm_inst").unwrap()];

            let outcome = grow_or_restore(
                deliver,
                set_of(&["bravo_arm_inst", "alpha_arm_inst"]),
                &alpha,
            )
            .await;

            assert_eq!(outcome, Ok(()));
            assert_eq!(
                *carried.lock().unwrap(),
                [vec![
                    "bravo_arm_inst".to_string(),
                    "alpha_arm_inst".to_string()
                ]],
                "one delivery, the grown set"
            );
        }

        #[tokio::test]
        async fn a_failed_delivery_puts_back_what_the_sets_held_before_the_copy() {
            let (deliver, carried) = recording(vec![Err("cannot reach `cn-robot`".into()), Ok(())]);
            let alpha = [Name::new("alpha_arm_inst").unwrap()];

            let outcome = grow_or_restore(
                deliver,
                set_of(&["bravo_arm_inst", "alpha_arm_inst"]),
                &alpha,
            )
            .await;

            let outcome = outcome.unwrap_err();
            assert!(
                outcome.starts_with("cannot reach `cn-robot`"),
                "the join fails on the delivery that did not land: {outcome}"
            );
            assert!(
                outcome.ends_with(
                    "The sets that took the copy's members hold what they held before it"
                ),
                "and says the restore landed: {outcome}"
            );
            assert_eq!(
                carried.lock().unwrap()[1],
                ["bravo_arm_inst".to_string()],
                "the restore carries the set without the copy's own members"
            );
        }

        #[tokio::test]
        async fn a_restore_that_fails_too_is_reported_with_what_heals_it() {
            let (deliver, _carried) = recording(vec![
                Err("cannot reach `cn-robot`".into()),
                Err("cannot reach `cn-robot`".into()),
            ]);
            let alpha = [Name::new("alpha_arm_inst").unwrap()];

            let outcome = grow_or_restore(deliver, set_of(&["alpha_arm_inst"]), &alpha)
                .await
                .unwrap_err();

            assert!(
                outcome.starts_with("cannot reach `cn-robot`"),
                "the delivery's own failure leads: {outcome}"
            );
            assert!(
                outcome.contains("restoring the sets they held failed"),
                "{outcome}"
            );
            assert!(outcome.contains(UNDELIVERED_REMEDY), "{outcome}");
        }
    }
}
