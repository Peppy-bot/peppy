//! Copies: one option of a `zero_or_more` axis composed under a name, its
//! ids minted under that name, and the way copies are folded into the
//! stack, at launch, at join and at removal.

use super::super::composition::{ArgumentOverrides, OriginatedAdjustment};
use super::super::types::{
    Deployment, DeploymentInstance, LauncherFramework, LinkTargets, LinkValue, PeppyLauncher,
    Selection, split_link_target,
};
use super::constraints::{self, ConstraintInPlay, ConstraintScope, names_axis};
use super::error::CompositionError;
use super::expand::{
    Expanded, OriginatedDeployment, Unit, append_links, expand_unit, instance_named_mut,
};
use super::load::{LoadedFragment, LoadedOption};
use super::prepared::PreparedLauncher;
use super::report::{AppliedAdjustment, AppliedChange, SkippedAdjustment, render, render_option};
use super::select::{CopyOrigin, UnitSelection, resolve_copy};
use config::runtime::{CoreNodeName, CoreNodeNameError, Name, instance_id_in_copy};
use core_node_api::encoding::{ArgumentOverride, SetMember};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// One copy on the stack: what it copies, how its axes were filled, and the
/// instances it minted.
#[derive(Debug, Clone)]
pub struct CopyRecord {
    pub name: Name,
    pub axis: String,
    pub option: String,
    pub selection: UnitSelection,
    pub instance_ids: Vec<Name>,
    /// The members the copy added to stack instances' set slots, in the order
    /// it added them. Removing the copy takes out exactly these.
    pub set_members: Vec<SetMember>,
}

/// The copy each instance of a stack belongs to, over the copies the stack
/// holds; an instance the launcher deploys outside any copy has no entry.
/// Built from the copy records at launch, at join and at removal, and read
/// wherever a plan names an instance by its copy: the members the validator
/// stamps into a bound set, and the copy a spawned instance is told it is in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopyMembership {
    by_instance: BTreeMap<String, Name>,
}

impl CopyMembership {
    /// The membership `copies` establish.
    pub fn of<'a>(copies: impl IntoIterator<Item = &'a CopyRecord>) -> Self {
        Self {
            by_instance: copies
                .into_iter()
                .flat_map(|copy| {
                    copy.instance_ids
                        .iter()
                        .map(move |id| (id.as_str().to_string(), copy.name.clone()))
                })
                .collect(),
        }
    }

    /// The copy `instance_id` belongs to, or `None` for an instance the
    /// launcher deploys outside any copy.
    pub fn copy_of(&self, instance_id: &str) -> Option<&Name> {
        self.by_instance.get(instance_id)
    }
}

/// One write a copy makes to a stack instance: the field and the value the
/// copy needs there.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct StackWriteEntry {
    pub instance: String,
    pub write: AppliedChange,
}

/// One copy composed: the instances it owns, minted under its name, and
/// what it writes to the stack it joins.
#[derive(Debug, Clone)]
pub(super) struct ComposedCopy {
    pub name: Name,
    pub axis: String,
    pub option: String,
    /// The copy's own axes: the copied option and the option's axes.
    pub selection: UnitSelection,
    pub instance_ids: Vec<Name>,
    /// The copy's deployments, ids minted, merged by source.
    pub deployments: Vec<Deployment>,
    /// The fields the copy's adjustments wrote on stack instances, as they
    /// must run for the copy to be what its fragments say.
    pub stack_writes: Vec<StackWriteEntry>,
    pub core_nodes: Vec<String>,
    pub applied: Vec<AppliedAdjustment>,
    pub skipped: Vec<SkippedAdjustment>,
}

/// Whether `target`, `instance` or `instance/link_id`, names one of
/// `instance_ids`.
fn names_one_of(instance_ids: &[Name], target: &str) -> bool {
    let (instance, _) = split_link_target(target);
    instance_ids.iter().any(|id| id.as_str() == instance)
}

impl CopyRecord {
    /// Whether `instance_id` is one of the copy's own instances.
    pub fn owns_instance(&self, instance_id: &str) -> bool {
        self.instance_ids
            .iter()
            .any(|id| id.as_str() == instance_id)
    }

    /// Whether `target`, `instance` or `instance/link_id`, names one of the
    /// copy's own instances.
    pub fn owns_target(&self, target: &str) -> bool {
        names_one_of(&self.instance_ids, target)
    }
}

impl ComposedCopy {
    /// Whether `target`, `instance` or `instance/link_id`, names one of the
    /// copy's own instances.
    fn owns(&self, target: &str) -> bool {
        names_one_of(&self.instance_ids, target)
    }

    pub(super) fn record(&self) -> CopyRecord {
        CopyRecord {
            name: self.name.clone(),
            axis: self.axis.clone(),
            option: self.option.clone(),
            selection: self.selection.clone(),
            instance_ids: self.instance_ids.clone(),
            set_members: self
                .stack_writes
                .iter()
                .filter_map(|entry| match &entry.write {
                    AppliedChange::LinkAdded { slot, target } => Some(SetMember {
                        instance_id: Name::new(&entry.instance).expect(
                            "a composed copy writes only to instances the stack runs, whose ids \
                             are names",
                        ),
                        link_id: slot.clone(),
                        target: target.clone(),
                    }),
                    _ => None,
                })
                .collect(),
        }
    }
}

/// What one copy is asked to be: the option to copy, the name to run
/// under, and the copy's own selection and overrides.
pub(super) struct CopyRequest<'a> {
    pub axis: &'a str,
    pub loaded: &'a LoadedOption,
    pub name: &'a Name,
    pub with: &'a BTreeMap<String, String>,
    pub arguments: &'a ArgumentOverrides,
    /// The copy's own adjustments, run after the launcher's and before
    /// `arguments`.
    pub adjustments: &'a [OriginatedAdjustment<'a>],
    pub origin: CopyOrigin,
}

/// A copy's name is its placement link, so it is held to a core node
/// name's grammar.
fn check_copy_name(name: &Name) -> Result<(), CompositionError> {
    CoreNodeName::new(name.as_str())
        .map(|_| ())
        .map_err(|error| CompositionError::CopyNameNotPlaceable {
            copy: name.to_string(),
            reason: match error {
                CoreNodeNameError::Reserved => SELF_COPY_NAME_REFUSAL.to_owned(),
                CoreNodeNameError::Malformed => error.to_string(),
            },
        })
}

/// Why `self` cannot name a copy, told to the file's author and to the
/// `stack join` caller alike.
pub const SELF_COPY_NAME_REFUSAL: &str =
    "`self` names the daemon a launch or join targets; give the copy another name";

/// One copy's selection `own` laid over the stack `stack`: the whole
/// selection, the fragments the copy pulls in and the constraints that
/// judge it. A launch, a join and `repo index --check` all derive the
/// three the same way, so the verdict is the same whichever path asks.
pub(super) struct CopyOverStack<'a> {
    pub(super) selection: UnitSelection,
    pub(super) fragments: Vec<&'a LoadedFragment>,
    pub(super) in_play: Vec<ConstraintInPlay<'a>>,
}

pub(super) fn copy_over_stack<'a>(
    launcher: &'a PeppyLauncher,
    stack: &UnitSelection,
    loaded: &'a LoadedOption,
    axis: &str,
    own: &UnitSelection,
) -> CopyOverStack<'a> {
    let selection = UnitSelection {
        entries: stack
            .launcher_entries(launcher)
            .into_iter()
            .chain(own.entries.iter().cloned())
            .collect(),
    };
    let fragments = loaded.fragments_for(own);
    let in_play =
        constraints::constraints_in_play(launcher, &fragments, ConstraintScope::Copy { axis });
    CopyOverStack {
        selection,
        fragments,
        in_play,
    }
}

/// The first clock domain a copy's fragments declare, with the document it
/// is written in. A launch's domains are the launcher's own and its stack
/// fragments'; a copy binds them and declares none.
fn declared_clock<'a>(fragments: &[&'a LoadedFragment]) -> Option<(&'a str, &'a Name)> {
    fragments.iter().copied().find_map(|fragment| {
        let (domain, _) = fragment.body.framework.clocks.first_key_value()?;
        Some((fragment.origin.as_str(), domain))
    })
}

/// Composes one copy over the stack `bare`, whose selection is `stack`.
/// `taken` holds the core node links already in use, which the name must
/// not be.
pub(super) fn compose_copy(
    prepared: &PreparedLauncher,
    stack: &UnitSelection,
    bare: &Expanded,
    request: CopyRequest<'_>,
    taken: &[String],
) -> Result<ComposedCopy, CompositionError> {
    let CopyRequest {
        axis,
        loaded,
        name,
        with,
        arguments,
        adjustments,
        origin,
    } = request;
    check_copy_name(name)?;
    if taken.iter().any(|link| link == name.as_str()) {
        return Err(CompositionError::NameIsCoreNodeLink {
            name: name.to_string(),
        });
    }
    let own = resolve_copy(loaded, axis, name.as_str(), with, origin)?;
    let launcher = &prepared.launcher;
    let CopyOverStack {
        selection,
        fragments,
        in_play,
    } = copy_over_stack(launcher, stack, loaded, axis, &own);
    if let Some((origin, domain)) = declared_clock(&fragments) {
        return Err(CompositionError::CopyFragmentDeclaresClock {
            origin: origin.to_owned(),
            copy: name.to_string(),
            domain: domain.to_string(),
        });
    }
    let owned: HashSet<String> = fragments
        .iter()
        .flat_map(|fragment| &fragment.body.deployments)
        .flat_map(|deployment| &deployment.instances)
        .map(|instance| instance.instance_id.to_string())
        .collect();
    if owned.is_empty() {
        return Err(CompositionError::CopyStartsNothing {
            copy: name.to_string(),
            option: loaded.name.clone(),
        });
    }
    let stack_ids: HashSet<&str> = bare
        .deployments
        .iter()
        .flat_map(|d| &d.instances)
        .map(|instance| instance.instance_id.as_str())
        .collect();
    if let Some(reused) = owned.iter().find(|id| stack_ids.contains(id.as_str())) {
        return Err(CompositionError::CopyReusesStackId {
            option: loaded.name.clone(),
            id: reused.clone(),
        });
    }

    constraints::check(&in_play, &selection)?;

    let base_adjustments = launcher
        .adjustments
        .iter()
        .filter(|adjustment| {
            names_axis(adjustment.when.as_ref(), axis) || owned.contains(adjustment.target.as_str())
        })
        .collect();
    let unit = Unit {
        base: bare
            .deployments
            .iter()
            .map(|deployment| OriginatedDeployment {
                deployment,
                origin: String::from("the stack"),
            })
            .collect(),
        fragments,
        base_adjustments,
        base_origin: prepared.base_origin(),
        copy_adjustments: adjustments.to_vec(),
        selection,
    };
    let mut expanded = expand_unit(&unit, &[])?;
    apply_overrides(name, &mut expanded, &owned, arguments)?;

    // Mint the owned ids under the name; every link naming one follows it.
    let mut minted: HashMap<String, Name> = HashMap::new();
    for id in &owned {
        let name_for = instance_id_in_copy(name, id);
        if stack_ids.contains(name_for.as_str()) {
            return Err(CompositionError::PrefixedIdCollision {
                copy: name.to_string(),
                id: name_for.to_string(),
            });
        }
        minted.insert(id.clone(), name_for);
    }
    let rewrite = |target: &str| -> String {
        let (id, suffix) = split_link_target(target);
        match (minted.get(id), suffix) {
            (Some(minted), Some(link)) => format!("{minted}/{link}"),
            (Some(minted), None) => minted.to_string(),
            (None, _) => target.to_owned(),
        }
    };

    let mut deployments: Vec<Deployment> = Vec::new();
    let mut instance_ids = Vec::new();
    for deployment in expanded.deployments {
        let mut owned_instances = Vec::new();
        for mut instance in deployment.instances {
            let Some(minted) = minted.get(instance.instance_id.as_str()) else {
                continue;
            };
            rewrite_links(name, &mut instance, &rewrite)?;
            if let Some(core_node) = &instance.core_node {
                return Err(CompositionError::CopyInstancePlaced {
                    copy: name.to_string(),
                    instance: instance.instance_id.to_string(),
                    core_node: core_node.clone(),
                });
            }
            instance.instance_id = minted.clone();
            instance.core_node = Some(name.to_string());
            instance_ids.push(minted.clone());
            owned_instances.push(instance);
        }
        if !owned_instances.is_empty() {
            deployments.push(Deployment {
                source: deployment.source,
                instances: owned_instances,
            });
        }
    }
    let mut core_nodes: Vec<String> = expanded.core_nodes;
    core_nodes.push(name.to_string());
    let applied = rewrite_report(name, expanded.applied, &minted, &rewrite)?;
    let stack_writes = applied
        .iter()
        .filter(|entry| !minted.values().any(|id| id.as_str() == entry.target))
        .map(|entry| StackWriteEntry {
            instance: entry.target.clone(),
            write: entry.change.clone(),
        })
        .collect();
    let skipped = expanded
        .skipped
        .into_iter()
        .map(|mut entry| {
            if let Some(minted) = minted.get(&entry.target) {
                entry.target = minted.to_string();
            }
            entry
        })
        .collect();
    Ok(ComposedCopy {
        name: name.clone(),
        axis: axis.to_owned(),
        option: loaded.name.clone(),
        selection: own,
        instance_ids,
        deployments,
        stack_writes,
        core_nodes,
        applied,
        skipped,
    })
}

/// The `--set-arguments` flags of one join as a copy's overrides.
pub(super) fn argument_overrides(
    arguments: &[ArgumentOverride],
) -> Result<ArgumentOverrides, CompositionError> {
    let mut overrides = ArgumentOverrides::new();
    for argument in arguments {
        let previous = overrides
            .entry(argument.instance_id().to_string())
            .or_default()
            .insert(argument.argument().to_string(), argument.value().clone());
        if previous.is_some() {
            return Err(CompositionError::DuplicateArgumentOverride {
                target: argument.instance_id().to_string(),
                argument: argument.argument().to_string(),
            });
        }
    }
    Ok(overrides)
}

/// Lays a copy's `arguments` over its own instances, after every
/// adjustment, each override reported like an adjustment.
fn apply_overrides(
    name: &Name,
    expanded: &mut Expanded,
    owned: &HashSet<String>,
    arguments: &ArgumentOverrides,
) -> Result<(), CompositionError> {
    let origin = format!("arguments of copy `{name}`");
    for (target, overrides) in arguments {
        if !owned.contains(target.as_str()) {
            return Err(CompositionError::ArgumentTargetAbsent {
                copy: name.to_string(),
                target: target.clone(),
                available: crate::error::format_quoted_list(owned.iter().collect::<BTreeSet<_>>()),
            });
        }
        let instance = instance_named_mut(&mut expanded.deployments, target)
            .expect("an owned id is an instance of the expanded copy");
        for (argument, value) in overrides {
            expanded.applied.push(AppliedAdjustment {
                target: target.clone(),
                change: AppliedChange::Argument {
                    key: argument.clone(),
                    old: instance.arguments.get(argument).cloned(),
                    new: value.clone(),
                },
                origin: origin.clone(),
            });
            instance.arguments.insert(argument.clone(), value.clone());
        }
    }
    Ok(())
}

fn rewrite_links(
    copy: &Name,
    instance: &mut DeploymentInstance,
    rewrite: &impl Fn(&str) -> String,
) -> Result<(), CompositionError> {
    for value in instance.links.values_mut() {
        rewrite_link(value, rewrite).map_err(|reason| CompositionError::PrefixedLinksCollide {
            copy: copy.to_string(),
            instance: instance.instance_id.to_string(),
            reason,
        })?;
    }
    Ok(())
}

fn rewrite_link(value: &mut LinkValue, rewrite: &impl Fn(&str) -> String) -> Result<(), String> {
    match value {
        LinkValue::Bound(Selection::Scalar(target)) => *target = rewrite(target),
        LinkValue::Bound(Selection::Array(targets) | Selection::Flags(targets)) => {
            *targets = LinkTargets::new(
                targets
                    .as_slice()
                    .iter()
                    .map(|target| rewrite(target))
                    .collect(),
            )
            .map_err(|e| e.to_string())?;
        }
        LinkValue::Vacant(_) => {}
    }
    Ok(())
}

/// Renames the report's targets and link values the way the plan was.
fn rewrite_report(
    copy: &Name,
    applied: Vec<AppliedAdjustment>,
    minted: &HashMap<String, Name>,
    rewrite: &impl Fn(&str) -> String,
) -> Result<Vec<AppliedAdjustment>, CompositionError> {
    applied
        .into_iter()
        .map(|mut entry| {
            if let Some(minted) = minted.get(&entry.target) {
                entry.target = minted.to_string();
            }
            let collide = |reason: String| CompositionError::PrefixedLinksCollide {
                copy: copy.to_string(),
                instance: entry.target.clone(),
                reason,
            };
            match &mut entry.change {
                AppliedChange::Argument { .. } | AppliedChange::Clock { .. } => {}
                AppliedChange::LinkSet { old, new, .. } => {
                    if let Some(old) = old {
                        rewrite_link(old, rewrite).map_err(collide)?;
                    }
                    rewrite_link(new, rewrite).map_err(collide)?;
                }
                AppliedChange::LinkAdded { target, .. } => *target = rewrite(target),
                AppliedChange::LinkRemoved { old, .. } => {
                    rewrite_link(old, rewrite).map_err(collide)?
                }
            }
            Ok(entry)
        })
        .collect()
}

/// Who wrote a stack field so far, for the copies-must-agree rule.
enum Claim {
    Value { copy: Name, rendered: String },
    Appended { copy: Name },
}

/// The stack with its copies folded in: each copy's writes to a stack
/// instance must agree with every other copy's, appended links are the
/// union, and its deployments merge in after the stack's own.
pub(super) fn combine(
    launcher: &PeppyLauncher,
    bare: &Expanded,
    framework: &LauncherFramework,
    copies: &[ComposedCopy],
) -> Result<PeppyLauncher, CompositionError> {
    let mut flat = flat_document(
        launcher,
        bare.deployments.clone(),
        bare.core_nodes.clone(),
        framework.clone(),
    );
    let mut claims: HashMap<(String, String), Claim> = HashMap::new();
    for copy in copies {
        for entry in &copy.stack_writes {
            let key = (entry.instance.clone(), entry.write.field());
            let conflict = |first: &Name, first_value: String| CompositionError::CopiesConflict {
                first: first.to_string(),
                second: copy.name.to_string(),
                target: format!("{}.{}", entry.instance, entry.write.field()),
                difference: format!(
                    "{first} writes {first_value}, {} writes {}",
                    copy.name,
                    entry.write.written()
                ),
            };
            match (claims.get(&key), &entry.write) {
                (
                    Some(Claim::Value {
                        copy: first,
                        rendered,
                    }),
                    write,
                ) => {
                    if *rendered != write.written() {
                        return Err(conflict(first, rendered.clone()));
                    }
                }
                (Some(Claim::Appended { .. }), AppliedChange::LinkAdded { .. }) => {}
                (Some(Claim::Appended { copy: first }), _) => {
                    return Err(conflict(first, String::from("appended producers")));
                }
                (None, _) => {}
            }
            let instance = instance_named_mut(&mut flat.deployments, &entry.instance)
                .expect("a copy writes only instances of the stack it composed over");
            let origin = format!("copy `{}`", copy.name);
            apply_write(instance, &entry.write, &origin)?;
            claims.insert(
                key,
                match &entry.write {
                    AppliedChange::LinkAdded { .. } => Claim::Appended {
                        copy: copy.name.clone(),
                    },
                    write => Claim::Value {
                        copy: copy.name.clone(),
                        rendered: write.written(),
                    },
                },
            );
        }
        add_copy(&mut flat, copy)?;
    }
    validate_flat(&flat)
}

/// Writes one field of a stack instance the way a copy needs it.
fn apply_write(
    instance: &mut DeploymentInstance,
    write: &AppliedChange,
    origin: &str,
) -> Result<(), CompositionError> {
    match write {
        AppliedChange::Argument { key, new, .. } => {
            instance.arguments.insert(key.clone(), new.clone());
        }
        AppliedChange::LinkSet { slot, new, .. } => {
            instance.links.insert(slot.clone(), new.clone());
        }
        AppliedChange::LinkRemoved { slot, .. } => {
            instance.links.remove(slot);
        }
        AppliedChange::LinkAdded { slot, target } => {
            let bound = append_links(
                instance,
                slot,
                std::slice::from_ref(target),
                instance.instance_id.as_str(),
                origin,
            )?;
            instance
                .links
                .insert(slot.clone(), LinkValue::Bound(Selection::Array(bound)));
        }
        AppliedChange::Clock { new, .. } => {
            instance.framework.clock = Some(new.clone());
        }
    }
    Ok(())
}

/// The running stack with one more copy: every field the copy writes on
/// a stack instance already holds that value, except a slot the instance
/// declares vacant, which the copy's relays pair into, and a set slot the
/// copy's own instances join.
pub(super) fn attach(
    existing: &PeppyLauncher,
    copy: &ComposedCopy,
) -> Result<PeppyLauncher, CompositionError> {
    let mut flat = existing.clone();
    for entry in &copy.stack_writes {
        let refusal = |changes: String| CompositionError::JoinChangesExisting {
            name: copy.name.to_string(),
            instance: entry.instance.clone(),
            changes,
        };
        let Some(running) = instance_named_mut(&mut flat.deployments, &entry.instance) else {
            return Err(refusal(String::from("the instance is not running")));
        };
        let field = entry.write.field();
        match &entry.write {
            AppliedChange::Argument {
                key, new: value, ..
            } => {
                let old = running.arguments.get(key);
                if old != Some(value) {
                    return Err(refusal(format!(
                        "{field}: {} -> {}",
                        render_option(old),
                        render(value)
                    )));
                }
            }
            AppliedChange::LinkSet {
                slot, new: value, ..
            } => {
                let old = running.links.get(slot);
                if old != Some(value) {
                    return Err(refusal(match old {
                        Some(LinkValue::Vacant(_)) => format!(
                            "{field} is vacant; pair into it from the copy's own instance \
                             `links` and release it with `unset_links`"
                        ),
                        _ => format!("{field}: {} -> {}", render_option(old), render(value)),
                    }));
                }
            }
            AppliedChange::LinkRemoved { slot, .. } => match running.links.get(slot) {
                None => {}
                Some(LinkValue::Vacant(_)) => {
                    if !pairs_into(copy, &entry.instance, slot) {
                        return Err(CompositionError::JoinReleasesUnpairedVacancy {
                            name: copy.name.to_string(),
                            instance: entry.instance.clone(),
                            slot: slot.clone(),
                        });
                    }
                    running.links.remove(slot);
                }
                Some(bound) => {
                    return Err(refusal(format!("{field}: {} -> (absent)", render(bound))));
                }
            },
            AppliedChange::LinkAdded { slot, target } => {
                if !copy.owns(target) {
                    return Err(CompositionError::JoinAddsStackMember {
                        name: copy.name.to_string(),
                        instance: entry.instance.clone(),
                        slot: slot.clone(),
                        target: target.clone(),
                    });
                }
                apply_write(running, &entry.write, &format!("copy `{}`", copy.name))?;
            }
            AppliedChange::Clock { new, .. } => {
                let old = running.framework.clock.as_ref();
                if old != Some(new) {
                    return Err(refusal(format!(
                        "{field}: {} -> {new}",
                        old.map_or_else(|| String::from("(absent)"), Name::to_string)
                    )));
                }
            }
        }
    }
    add_copy(&mut flat, copy)?;
    validate_flat(&flat)
}

/// The applied changes with each `old` read off the running stack, for a
/// join's report: the copy composes over the bare stack and joins what
/// runs. A release of a slot the running instance no longer has wrote
/// nothing and is left out.
pub(super) fn against_running(
    existing: &PeppyLauncher,
    applied: Vec<AppliedAdjustment>,
) -> Vec<AppliedAdjustment> {
    applied
        .into_iter()
        .filter_map(|mut entry| {
            let running = existing
                .deployments
                .iter()
                .flat_map(|deployment| &deployment.instances)
                .find(|instance| instance.instance_id.as_str() == entry.target);
            let Some(running) = running else {
                return Some(entry);
            };
            match &mut entry.change {
                AppliedChange::Argument { key, old, .. } => {
                    *old = running.arguments.get(key).cloned();
                }
                AppliedChange::LinkSet { slot, old, .. } => {
                    *old = running.links.get(slot).cloned();
                }
                AppliedChange::LinkRemoved { slot, old } => match running.links.get(slot) {
                    Some(value) => *old = value.clone(),
                    None => return None,
                },
                AppliedChange::Clock { old, .. } => {
                    *old = running.framework.clock.clone();
                }
                AppliedChange::LinkAdded { .. } => {}
            }
            Some(entry)
        })
        .collect()
}

/// Writes back the vacancies the stack declares, except on the slots the
/// copies that remain have released.
///
/// Releasing a vacancy is the one write that changes a stack instance
/// ([`attach`] verifies every other write against what already runs), so a
/// copy's instances leaving is undone by taking the slot's value from the
/// stack the launch composed: the same reason a bare stack boots with, which
/// [`super::super::pairings`] reads as the slot's cover. A slot another copy
/// released stays as that copy needs it.
fn restore_vacancies(
    remaining: &mut PeppyLauncher,
    bare: &PeppyLauncher,
    released: &HashSet<(String, String)>,
) {
    let vacancies: Vec<(String, String, LinkValue)> = bare
        .deployments
        .iter()
        .flat_map(|deployment| &deployment.instances)
        .flat_map(|instance| {
            instance.links.iter().filter_map(|(slot, value)| {
                value.vacancy()?;
                Some((
                    instance.instance_id.to_string(),
                    slot.clone(),
                    value.clone(),
                ))
            })
        })
        .filter(|(instance, slot, _)| !released.contains(&(instance.clone(), slot.clone())))
        .collect();
    for (instance, slot, value) in vacancies {
        if let Some(running) = instance_named_mut(&mut remaining.deployments, &instance) {
            running.links.entry(slot).or_insert(value);
        }
    }
}

/// The stack slots a composed copy releases to pair into them itself.
pub(super) fn released_vacancies(copy: &ComposedCopy) -> impl Iterator<Item = (String, String)> {
    copy.stack_writes
        .iter()
        .filter_map(|entry| match &entry.write {
            AppliedChange::LinkRemoved { slot, .. } => Some((entry.instance.clone(), slot.clone())),
            _ => None,
        })
}

/// Whether one of the copy's instances links into `slot` of `instance`,
/// by `instance/slot` or by the instance alone.
fn pairs_into(copy: &ComposedCopy, instance: &str, slot: &str) -> bool {
    copy.deployments
        .iter()
        .flat_map(|deployment| &deployment.instances)
        .flat_map(|owned| owned.links.values())
        .filter_map(LinkValue::selection)
        .flat_map(Selection::targets)
        .any(|target| {
            let (target_instance, target_slot) = split_link_target(target);
            target_instance == instance && target_slot.is_none_or(|named| named == slot)
        })
}

/// The running stack without one copy: its instances, the members it added
/// to stack sets, and its placement link gone. A stack instance linking to
/// one of the copy's instances through any other link keeps the copy.
pub(super) fn detach(
    existing: &PeppyLauncher,
    copy: &CopyRecord,
    bare: &PeppyLauncher,
    released: &HashSet<(String, String)>,
) -> Result<PeppyLauncher, CompositionError> {
    let removed: HashSet<&str> = copy.instance_ids.iter().map(Name::as_str).collect();
    let mut remaining = existing.clone();
    for deployment in &mut remaining.deployments {
        deployment
            .instances
            .retain(|instance| !removed.contains(instance.instance_id.as_str()));
    }
    remaining
        .deployments
        .retain(|deployment| !deployment.instances.is_empty());
    remaining
        .core_nodes
        .retain(|link| link != copy.name.as_str());
    for member in &copy.set_members {
        // A copy composes over the bare stack, and a removal drops only the
        // copy's own instances, so the instance a member joined is still here.
        let running = instance_named_mut(&mut remaining.deployments, member.instance_id.as_str())
            .expect("a copy's members join bare-stack instances, which a removal never drops");
        drop_member(running, member);
    }
    let mut linked: Vec<String> = Vec::new();
    for instance in remaining
        .deployments
        .iter()
        .flat_map(|deployment| &deployment.instances)
    {
        for (slot, value) in &instance.links {
            let Some(selection) = value.selection() else {
                continue;
            };
            for target in selection.targets() {
                if removed.contains(split_link_target(target).0) {
                    linked.push(format!("{}.{slot} -> {target}", instance.instance_id));
                }
            }
        }
    }
    if !linked.is_empty() {
        return Err(CompositionError::CopyStillLinked {
            copy: copy.name.to_string(),
            links: linked.join(", "),
        });
    }
    restore_vacancies(&mut remaining, bare, released);
    validate_flat(&remaining)
}

/// Takes one member a copy added out of the set slot it joined. Panics when
/// the slot is not bound as an array; a slot without the member is left as it
/// is.
fn drop_member(instance: &mut DeploymentInstance, member: &SetMember) {
    let Some(LinkValue::Bound(Selection::Array(targets))) = instance.links.get(&member.link_id)
    else {
        panic!(
            "`{}.links.{}` holds a member of a copy, so it holds an array",
            instance.instance_id, member.link_id
        );
    };
    let kept = LinkTargets::new(
        targets
            .as_slice()
            .iter()
            .filter(|target| **target != member.target)
            .cloned()
            .collect(),
    )
    .expect("a subset of a duplicate-free target list is duplicate-free");
    instance.links.insert(
        member.link_id.clone(),
        LinkValue::Bound(Selection::Array(kept)),
    );
}

fn add_copy(flat: &mut PeppyLauncher, copy: &ComposedCopy) -> Result<(), CompositionError> {
    let present: HashSet<String> = flat
        .deployments
        .iter()
        .flat_map(|d| &d.instances)
        .map(|instance| instance.instance_id.to_string())
        .collect();
    for id in &copy.instance_ids {
        if present.contains(id.as_str()) {
            return Err(CompositionError::PrefixedIdCollision {
                copy: copy.name.to_string(),
                id: id.to_string(),
            });
        }
    }
    for deployment in &copy.deployments {
        match flat
            .deployments
            .iter_mut()
            .find(|entry| entry.source == deployment.source)
        {
            Some(entry) => entry.instances.extend(deployment.instances.iter().cloned()),
            None => flat.deployments.push(deployment.clone()),
        }
    }
    for link in &copy.core_nodes {
        if !flat.core_nodes.contains(link) {
            flat.core_nodes.push(link.clone());
        }
    }
    Ok(())
}

/// The flat document a composition produces: the launcher's schema, the
/// deployments and links given, nothing left to select.
pub(super) fn flat_document(
    launcher: &PeppyLauncher,
    deployments: Vec<Deployment>,
    core_nodes: Vec<String>,
    framework: LauncherFramework,
) -> PeppyLauncher {
    PeppyLauncher {
        peppy_schema: launcher.peppy_schema,
        core_nodes,
        deployments,
        option_deployments: Vec::new(),
        components: Vec::new(),
        adjustments: Vec::new(),
        constraints: Vec::new(),
        framework,
    }
}

/// The flattened document goes back through the flat launcher's own
/// validation (serialize, re-parse), so every whole-document check a
/// hand-written file gets runs on the composed result too.
pub(super) fn validate_flat(flat: &PeppyLauncher) -> Result<PeppyLauncher, CompositionError> {
    let text = serde_json5::to_string(flat).map_err(|e| {
        CompositionError::FlatValidation(crate::error::Error::Serialize(e.to_string()))
    })?;
    Ok(super::super::parse::PeppyLauncherParser::from_content(
        &text,
    )?)
}

#[cfg(test)]
mod membership_tests {
    use super::*;

    fn copy(name: &str, instance_ids: &[&str]) -> CopyRecord {
        CopyRecord {
            name: Name::new(name).unwrap(),
            axis: "robot".into(),
            option: "real".into(),
            selection: UnitSelection::default(),
            instance_ids: instance_ids
                .iter()
                .map(|id| Name::new(*id).unwrap())
                .collect(),
            set_members: Vec::new(),
        }
    }

    /// Every instance a copy minted answers that copy; an instance outside
    /// every copy answers none.
    #[test]
    fn each_minted_instance_answers_its_copy() {
        let membership = CopyMembership::of(&[
            copy("alpha", &["alpha_arm_inst", "alpha_leader_inst"]),
            copy("bravo", &["bravo_arm_inst"]),
        ]);
        assert_eq!(
            membership.copy_of("alpha_leader_inst").map(Name::as_str),
            Some("alpha")
        );
        assert_eq!(
            membership.copy_of("bravo_arm_inst").map(Name::as_str),
            Some("bravo")
        );
        assert_eq!(membership.copy_of("hub_inst"), None);
        assert_eq!(CopyMembership::default().copy_of("alpha_arm_inst"), None);
    }
}
