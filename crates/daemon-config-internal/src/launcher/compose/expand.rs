//! Expanding one unit of a launch, the stack or one copy: its deployments
//! collected and merged by source, its adjustments planned and applied in
//! order, and the report of what they did.

use super::super::composition::OriginatedAdjustment;
use super::super::types::{
    ClockDeclaration, Deployment, DeploymentInstance, LauncherFramework, LinkTargets, LinkValue,
    Selection, WALL_CLOCK,
};
use super::constraints::{guard_holds, render_guard};
use super::error::{ClockDomainConflict, CompositionError};
use super::load::LoadedFragment;
use super::report::{AppliedAdjustment, AppliedChange, SkipReason, SkippedAdjustment};
use super::select::UnitSelection;
use config::runtime::Name;
use std::collections::{BTreeMap, HashMap};

/// One deployment with the label of the document it came from.
#[derive(Clone)]
pub(super) struct OriginatedDeployment<'a> {
    pub deployment: &'a Deployment,
    pub origin: String,
}

/// What one unit expands: the deployments already in its scope, the
/// fragments the selection pulled in, and the launcher's adjustments that
/// speak about it.
pub(super) struct Unit<'a> {
    /// Deployments in scope before any fragment: the launcher's own for the
    /// stack, the composed stack's for a copy.
    pub base: Vec<OriginatedDeployment<'a>>,
    pub fragments: Vec<&'a LoadedFragment>,
    /// The launcher's adjustments reaching this unit, in the order the
    /// launcher applies them.
    pub launcher_adjustments: Vec<OriginatedAdjustment<'a>>,
    /// A copy's own adjustments; they run after the launcher's.
    pub copy_adjustments: Vec<OriginatedAdjustment<'a>>,
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

/// Collects the unit's deployments, base first, then each fragment's in
/// order, and applies the adjustments: each fragment's own in collection
/// order, the launcher's in list order, then a copy's own. An adjustment
/// runs only if its guard holds and its target is in this unit; each skip
/// is recorded.
pub(super) fn expand_unit(
    unit: &Unit<'_>,
    core_nodes: &[String],
) -> Result<Expanded, CompositionError> {
    let collected = collect_deployments(unit);
    let defined = defined_once(&collected)?;
    let mut merged = merge_by_source(&collected);
    let links = union_core_nodes(unit, core_nodes);
    let (running, skipped) = plan_adjustments(unit, &defined);
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

/// The domains `base` and every selected fragment declare, merged. Each
/// domain is held with the document it came from, so a refusal names the
/// document that declared what it quotes.
///
/// Two documents may declare one domain, and identical declarations agree:
/// a robot fragment and the simulation it runs in can both name the same
/// timeline. Declarations that differ are refused naming both documents,
/// because nothing about composition order makes one of them right.
pub(super) fn merge_clocks(
    base_origin: &str,
    base: &LauncherFramework,
    fragments: &[&LoadedFragment],
) -> Result<LauncherFramework, CompositionError> {
    let mut declared: BTreeMap<Name, (String, ClockDeclaration)> = BTreeMap::new();
    let contributions = std::iter::once((base_origin, base)).chain(
        fragments
            .iter()
            .map(|fragment| (fragment.origin.as_str(), &fragment.body.framework)),
    );
    for (origin, framework) in contributions {
        for (domain, declaration) in &framework.clocks {
            match declared.get(domain) {
                Some((_, existing)) if existing == declaration => {}
                Some((first_origin, existing)) => {
                    return Err(CompositionError::ClockDomainConflict(Box::new(
                        ClockDomainConflict {
                            domain: domain.to_string(),
                            first_origin: first_origin.clone(),
                            first: render_clock(existing),
                            second_origin: origin.to_owned(),
                            second: render_clock(declaration),
                        },
                    )));
                }
                None => {
                    declared.insert(domain.clone(), (origin.to_owned(), declaration.clone()));
                }
            }
        }
    }
    Ok(LauncherFramework {
        clocks: declared
            .into_iter()
            .map(|(domain, (_, declaration))| (domain, declaration))
            .collect(),
    })
}

/// One declaration as a refusal quotes it back.
fn render_clock(declaration: &ClockDeclaration) -> String {
    match declaration {
        ClockDeclaration::Wall => format!("\"{WALL_CLOCK}\""),
        ClockDeclaration::Sim { publisher } => format!("{{ publisher: \"{publisher}\" }}"),
    }
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

/// The adjustments that run, in order, and the ones skipped with their
/// reason. The order is what decides a field two entries both write, the
/// later one winning: each fragment's own entries in collection order, then
/// the launcher's, its options' in the order `components` declares their
/// axes and then its top-level list, then the copy's own.
fn plan_adjustments<'a>(
    unit: &'a Unit<'a>,
    defined: &HashMap<&str, &str>,
) -> (Vec<OriginatedAdjustment<'a>>, Vec<SkippedAdjustment>) {
    let planned = unit
        .fragments
        .iter()
        .flat_map(|fragment| {
            fragment
                .body
                .adjustments
                .iter()
                .map(|adjustment| OriginatedAdjustment {
                    adjustment,
                    origin: fragment.origin.clone(),
                })
        })
        .chain(
            unit.launcher_adjustments
                .iter()
                .chain(&unit.copy_adjustments)
                .cloned(),
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
    step: &OriginatedAdjustment<'_>,
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
    if let Some(framework) = &step.adjustment.set_framework
        && let Some(clock) = &framework.clock
    {
        record(AppliedChange::Clock {
            old: instance.framework.clock.clone(),
            new: clock.clone(),
        });
        instance.framework.clock = Some(clock.clone());
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
