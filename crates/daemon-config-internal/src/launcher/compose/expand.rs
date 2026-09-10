//! Expanding one unit of a launch, the stack or one copy: its deployments
//! collected and merged by source, its adjustments planned and applied in
//! order, and the report of what they did.

use super::super::composition::Adjustment;
use super::super::types::{Deployment, DeploymentInstance, LinkTargets, LinkValue, Selection};
use super::constraints::{guard_holds, render_guard};
use super::error::CompositionError;
use super::load::LoadedFragment;
use super::report::{AppliedAdjustment, AppliedChange, SkipReason, SkippedAdjustment};
use super::select::UnitSelection;
use std::collections::{BTreeMap, HashMap, btree_map::Entry};

/// One deployment with the label of the document it came from.
#[derive(Clone)]
pub(super) struct OriginatedDeployment<'a> {
    pub deployment: &'a Deployment,
    pub origin: String,
}

/// What one unit expands: the deployments already in its scope, the
/// fragments the selection pulled in, and the base adjustments that speak
/// about it.
pub(super) struct Unit<'a> {
    /// Deployments in scope before any fragment: the launcher's own for the
    /// stack, the composed stack's for a copy.
    pub base: Vec<OriginatedDeployment<'a>>,
    pub fragments: Vec<&'a LoadedFragment>,
    pub base_adjustments: Vec<&'a Adjustment>,
    pub base_origin: String,
    pub selection: UnitSelection,
}

/// One unit expanded: its deployments merged by source with every adjustment
/// applied, ids as written.
#[derive(Debug, Clone)]
pub(super) struct Expanded {
    pub deployments: Vec<Deployment>,
    pub core_nodes: Vec<String>,
    pub applied: Vec<AppliedAdjustment>,
    pub skipped: Vec<SkippedAdjustment>,
}

/// One adjustment paired with where it came from and whether it speaks for
/// a fragment (bound by the no-fighting rules) or for the base (which may
/// override anything a fragment set).
struct PlannedAdjustment<'a> {
    adjustment: &'a Adjustment,
    origin: String,
    /// Identifies the fragment an adjustment belongs to for the conflict
    /// rules: two adjustments from one fragment are one author applying list
    /// order, two from different fragments are two authors fighting. `None`
    /// marks the base, the author who owns the file, which joins no
    /// conflict at all.
    fragment_id: Option<usize>,
}

/// Collects the unit's deployments, base first, then each fragment's in
/// order, and applies its adjustments: fragments in collection order, then
/// the base in list order. An adjustment runs only if its guard holds and its
/// target is in this unit; each skip is recorded.
pub(super) fn expand_unit(
    unit: &Unit<'_>,
    core_nodes: &[String],
) -> Result<Expanded, CompositionError> {
    let collected = collect_deployments(unit);
    let defined = defined_once(&collected)?;
    let mut merged = merge_by_source(&collected);
    let links = union_core_nodes(unit, core_nodes);
    let (running, skipped) = plan_adjustments(unit, &defined);
    check_conflicts(&running)?;
    let mut applied = Vec::new();
    for step in &running {
        let instance = instance_named(&mut merged, step.adjustment.target.as_str());
        apply_adjustment(instance, step, &mut applied)?;
    }
    Ok(Expanded {
        deployments: merged,
        core_nodes: links,
        applied,
        skipped,
    })
}

/// The unit's deployments in collection order, each with the label of the
/// document it came from.
fn collect_deployments<'a>(unit: &Unit<'a>) -> Vec<OriginatedDeployment<'a>> {
    let from_fragments = unit.fragments.iter().flat_map(|fragment| {
        fragment
            .body
            .deployments
            .iter()
            .map(|deployment| OriginatedDeployment {
                deployment,
                origin: fragment.origin.clone(),
            })
    });
    unit.base.iter().cloned().chain(from_fragments).collect()
}

/// Every instance id once across the collected set, with the document
/// defining it.
fn defined_once<'a>(
    collected: &'a [OriginatedDeployment<'_>],
) -> Result<HashMap<&'a str, &'a str>, CompositionError> {
    let mut first_origin: HashMap<&str, &str> = HashMap::new();
    for entry in collected {
        for instance in &entry.deployment.instances {
            let id = instance.instance_id.as_str();
            if let Some(first) = first_origin.insert(id, &entry.origin) {
                return Err(CompositionError::DuplicateInstanceId {
                    id: id.to_owned(),
                    first: first.to_owned(),
                    second: entry.origin.clone(),
                });
            }
        }
    }
    Ok(first_origin)
}

/// Entries sharing a source merge into one whose instance list is the
/// union, in collection order. The daemon resolves one deployment per
/// name:tag, so this merge is what lets a base and a fragment both deploy
/// `uvc_camera_linux`.
fn merge_by_source(collected: &[OriginatedDeployment<'_>]) -> Vec<Deployment> {
    let mut merged: Vec<Deployment> = Vec::with_capacity(collected.len());
    for &OriginatedDeployment { deployment, .. } in collected {
        match merged
            .iter_mut()
            .find(|entry| entry.source == deployment.source)
        {
            Some(entry) => entry.instances.extend(deployment.instances.iter().cloned()),
            None => merged.push(deployment.clone()),
        }
    }
    merged
}

/// Core node links: base first, then fragments in collection order; a link
/// declared by both is one link.
fn union_core_nodes(unit: &Unit<'_>, core_nodes: &[String]) -> Vec<String> {
    let mut links: Vec<String> = core_nodes.to_vec();
    for fragment in &unit.fragments {
        for link in &fragment.body.core_nodes {
            if !links.contains(link) {
                links.push(link.clone());
            }
        }
    }
    links
}

/// The adjustments that run, in order (fragments in collection order, then
/// the base), and the ones skipped with their reason.
fn plan_adjustments<'a>(
    unit: &Unit<'a>,
    defined: &HashMap<&str, &str>,
) -> (Vec<PlannedAdjustment<'a>>, Vec<SkippedAdjustment>) {
    let planned = unit
        .fragments
        .iter()
        .flat_map(|fragment| {
            fragment
                .body
                .adjustments
                .iter()
                .map(|adjustment| PlannedAdjustment {
                    adjustment,
                    origin: fragment.origin.clone(),
                    fragment_id: Some(fragment.id),
                })
        })
        .chain(
            unit.base_adjustments
                .iter()
                .map(|adjustment| PlannedAdjustment {
                    adjustment,
                    origin: unit.base_origin.clone(),
                    fragment_id: None,
                }),
        );
    let mut running = Vec::new();
    let mut skipped = Vec::new();
    for step in planned {
        if let Some(when) = &step.adjustment.when
            && !guard_holds(&unit.selection, when)
        {
            skipped.push(SkippedAdjustment {
                target: step.adjustment.target.to_string(),
                reason: SkipReason::GuardNotMet(render_guard(when)),
                origin: step.origin,
            });
            continue;
        }
        if !defined.contains_key(step.adjustment.target.as_str()) {
            skipped.push(SkippedAdjustment {
                target: step.adjustment.target.to_string(),
                reason: SkipReason::TargetAbsent,
                origin: step.origin,
            });
            continue;
        }
        running.push(step);
    }
    (running, skipped)
}

fn instance_named<'a>(merged: &'a mut [Deployment], target: &str) -> &'a mut DeploymentInstance {
    instance_named_mut(merged, target)
        .expect("a running adjustment targets an instance the unit defines")
}

pub(super) fn instance_named_mut<'a>(
    deployments: &'a mut [Deployment],
    id: &str,
) -> Option<&'a mut DeploymentInstance> {
    deployments
        .iter_mut()
        .flat_map(|deployment| deployment.instances.iter_mut())
        .find(|instance| instance.instance_id.as_str() == id)
}

/// Applies one adjustment's verbs to its target, in the order the grammar
/// lists them, recording each field written.
fn apply_adjustment(
    instance: &mut DeploymentInstance,
    step: &PlannedAdjustment<'_>,
    applied: &mut Vec<AppliedAdjustment>,
) -> Result<(), CompositionError> {
    let target = step.adjustment.target.to_string();
    let origin = step.origin.clone();
    let mut record = |change: AppliedChange| {
        applied.push(AppliedAdjustment {
            target: target.clone(),
            change,
            origin: origin.clone(),
        });
    };
    if let Some(arguments) = &step.adjustment.set_arguments {
        for (key, value) in arguments {
            record(AppliedChange::Argument {
                key: key.clone(),
                old: instance.arguments.get(key).cloned(),
                new: value.clone(),
            });
            instance.arguments.insert(key.clone(), value.clone());
        }
    }
    if let Some(links) = &step.adjustment.set_links {
        for (slot, value) in links {
            record(AppliedChange::LinkSet {
                slot: slot.clone(),
                old: instance.links.get(slot).cloned(),
                new: value.clone(),
            });
            instance.links.insert(slot.clone(), value.clone());
        }
    }
    if let Some(links) = &step.adjustment.add_links {
        for (slot, additions) in links {
            let bound = append_links(instance, slot, additions, &target, &origin)?;
            for addition in additions {
                record(AppliedChange::LinkAdded {
                    slot: slot.clone(),
                    target: addition.clone(),
                });
            }
            instance
                .links
                .insert(slot.clone(), LinkValue::Bound(Selection::Array(bound)));
        }
    }
    if let Some(slots) = &step.adjustment.unset_links {
        for slot in slots {
            if let Some(old) = instance.links.remove(slot) {
                record(AppliedChange::LinkRemoved {
                    slot: slot.clone(),
                    old,
                });
            }
        }
    }
    Ok(())
}

/// Appending is for array bindings: an absent slot starts one, an existing
/// array extends, anything else is refused, as is a producer already bound.
pub(super) fn append_links(
    instance: &DeploymentInstance,
    slot: &str,
    additions: &[String],
    target: &str,
    origin: &str,
) -> Result<LinkTargets, CompositionError> {
    let refuse = |holds: &'static str| CompositionError::AddLinksOnNonArray {
        origin: origin.to_owned(),
        target: target.to_owned(),
        slot: slot.to_owned(),
        holds,
    };
    let mut targets = match instance.links.get(slot) {
        None => Vec::new(),
        Some(LinkValue::Bound(Selection::Array(existing))) => existing.as_slice().to_vec(),
        Some(LinkValue::Bound(Selection::Scalar(_))) => return Err(refuse("a scalar binding")),
        Some(LinkValue::Bound(Selection::Flags(_))) => {
            return Err(refuse("a flag-accumulated binding"));
        }
        Some(LinkValue::Vacant(_)) => return Err(refuse("a vacancy")),
    };
    for addition in additions {
        if targets.contains(addition) {
            return Err(CompositionError::AddLinksDuplicateTarget {
                origin: origin.to_owned(),
                target: target.to_owned(),
                slot: slot.to_owned(),
                added: addition.clone(),
            });
        }
        targets.push(addition.clone());
    }
    LinkTargets::new(targets).map_err(|err| CompositionError::AddLinksDuplicateTarget {
        origin: origin.to_owned(),
        target: target.to_owned(),
        slot: slot.to_owned(),
        added: err.target,
    })
}

/// The two key spaces an adjustment can write in. `add_links` shares the
/// links space with `set_links` and `unset_links`: appending to a slot and
/// replacing it are two claims on the same entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum KeySpace {
    Arguments,
    Links,
}

impl KeySpace {
    fn label(self) -> &'static str {
        match self {
            KeySpace::Arguments => "arguments",
            KeySpace::Links => "links",
        }
    }
}

/// The conflict rules among the surviving fragment adjustments: a field
/// is written by one fragment. The base owns the file, so its later entries
/// win over its earlier ones and over any fragment's. Both maps are ordered
/// so when several pairs fight, the one reported is the same on every run.
fn check_conflicts(running: &[PlannedAdjustment<'_>]) -> Result<(), CompositionError> {
    let mut writers: BTreeMap<(&str, KeySpace, &str), (Option<usize>, String)> = BTreeMap::new();
    let mut adders: BTreeMap<(&str, &str), String> = BTreeMap::new();
    for step in running {
        if step.fragment_id.is_none() {
            continue;
        }
        let target = step.adjustment.target.as_str();
        let written_keys = step
            .adjustment
            .set_arguments
            .iter()
            .flat_map(|arguments| arguments.keys())
            .map(|key| (KeySpace::Arguments, key.as_str()))
            .chain(
                step.adjustment
                    .set_links
                    .iter()
                    .flat_map(|links| links.keys())
                    .map(|key| (KeySpace::Links, key.as_str())),
            )
            .chain(
                step.adjustment
                    .unset_links
                    .iter()
                    .flatten()
                    .map(|key| (KeySpace::Links, key.as_str())),
            );
        for (space, key) in written_keys {
            claim_write(&mut writers, target, space, key, step)?;
        }
        for key in step
            .adjustment
            .add_links
            .iter()
            .flat_map(|links| links.keys())
        {
            adders.insert((target, key.as_str()), step.origin.clone());
        }
    }
    // Appending to a slot and replacing it are two claims on the same
    // entry, from one fragment or two, and the pair is refused whichever
    // order it would apply in.
    for ((target, key), adder_origin) in &adders {
        if let Some((_writer_id, writer_origin)) = writers.get(&(target, KeySpace::Links, key)) {
            return Err(CompositionError::AdjustmentsConflict {
                target: (*target).to_owned(),
                field: format!("links.{key}"),
                first: writer_origin.clone(),
                second: adder_origin.clone(),
            });
        }
    }
    Ok(())
}

/// Records one fragment's claim on a `(target, key)` write slot, refusing
/// when a different fragment already claimed it. Two adjustments from one
/// fragment are one author applying list order, so the later supersedes
/// the earlier's claim.
fn claim_write<'a>(
    writers: &mut BTreeMap<(&'a str, KeySpace, &'a str), (Option<usize>, String)>,
    target: &'a str,
    space: KeySpace,
    key: &'a str,
    step: &PlannedAdjustment<'_>,
) -> Result<(), CompositionError> {
    match writers.entry((target, space, key)) {
        Entry::Vacant(slot) => {
            slot.insert((step.fragment_id, step.origin.clone()));
        }
        Entry::Occupied(mut slot) => {
            let (claimed_by, first) = slot.get_mut();
            if *claimed_by != step.fragment_id {
                return Err(CompositionError::AdjustmentsConflict {
                    target: target.to_owned(),
                    field: format!("{}.{key}", space.label()),
                    first: first.clone(),
                    second: step.origin.clone(),
                });
            }
            *first = step.origin.clone();
        }
    }
    Ok(())
}
