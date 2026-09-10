//! Copies: one option of a `zero_or_more` axis composed under a name, its
//! ids minted under that name, and the way copies are folded into the
//! stack, at launch, at join and at removal.

use super::super::composition::ArgumentOverrides;
use super::super::types::{
    Deployment, DeploymentInstance, LinkTargets, LinkValue, PeppyLauncher, Selection,
    split_link_target,
};
use super::constraints::{self, ConstraintScope, names_axis};
use super::error::CompositionError;
use super::expand::{
    Expanded, OriginatedDeployment, Unit, append_links, expand_unit, instance_named_mut,
};
use super::load::LoadedOption;
use super::prepared::PreparedLauncher;
use super::report::{AppliedAdjustment, AppliedChange, SkippedAdjustment, render, render_option};
use super::select::{UnitSelection, resolve_copy};
use config::{
    AnyType,
    runtime::{CoreNodeName, Name},
};
use core_node_api::encoding::ArgumentOverride;
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
}

/// One field a copy writes on a stack instance, and what it needs there.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum StackWrite {
    Argument { key: String, value: AnyType },
    SetLink { slot: String, value: LinkValue },
    AddLink { slot: String, target: String },
    UnsetLink { slot: String },
}

impl StackWrite {
    fn field(&self) -> String {
        match self {
            StackWrite::Argument { key, .. } => format!("arguments.{key}"),
            StackWrite::SetLink { slot, .. }
            | StackWrite::AddLink { slot, .. }
            | StackWrite::UnsetLink { slot } => format!("links.{slot}"),
        }
    }

    /// The value the write leaves in its field, as a conflict names it.
    fn render(&self) -> String {
        match self {
            StackWrite::Argument { value, .. } => render(value),
            StackWrite::SetLink { value, .. } => render(value),
            StackWrite::AddLink { target, .. } => format!("+ {target}"),
            StackWrite::UnsetLink { .. } => String::from("(absent)"),
        }
    }
}

/// One write a copy makes to a stack instance.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct StackWriteEntry {
    pub instance: String,
    pub write: StackWrite,
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

impl ComposedCopy {
    pub(super) fn record(&self) -> CopyRecord {
        CopyRecord {
            name: self.name.clone(),
            axis: self.axis.clone(),
            option: self.option.clone(),
            selection: self.selection.clone(),
            instance_ids: self.instance_ids.clone(),
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
}

/// A copy's name is its placement link, so it is held to a core node
/// name's grammar.
fn check_copy_name(name: &Name) -> Result<(), CompositionError> {
    CoreNodeName::new(name.as_str())
        .map(|_| ())
        .map_err(|error| CompositionError::CopyNameNotPlaceable {
            copy: name.to_string(),
            reason: error.to_string(),
        })
}

/// One of a copy's ids, minted under its name. Both halves are names and
/// `_` is a name character, so the join is a name.
fn prefixed_id(copy: &Name, id: &str) -> Name {
    Name::try_from(format!("{copy}_{id}")).expect("two names joined by `_` are a name")
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
    } = request;
    check_copy_name(name)?;
    if taken.iter().any(|link| link == name.as_str()) {
        return Err(CompositionError::NameIsCoreNodeLink {
            name: name.to_string(),
        });
    }
    let own = resolve_copy(loaded, axis, name.as_str(), with)?;
    let selection = UnitSelection {
        entries: stack
            .launcher_entries(&prepared.launcher)
            .into_iter()
            .chain(own.entries.iter().cloned())
            .collect(),
    };
    let fragments = loaded.fragments_for(&own);
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

    let launcher = &prepared.launcher;
    let in_play =
        constraints::constraints_in_play(launcher, &fragments, ConstraintScope::Copy { axis });
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
        base_origin: format!("{} (base)", prepared.label),
        selection,
    };
    let mut expanded = expand_unit(&unit, &[])?;
    apply_overrides(name, &mut expanded, &owned, arguments)?;

    // Mint the owned ids under the name; every link naming one follows it.
    let mut minted: HashMap<String, Name> = HashMap::new();
    for id in &owned {
        let name_for = prefixed_id(name, id);
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
            write: match &entry.change {
                AppliedChange::Argument { key, new, .. } => StackWrite::Argument {
                    key: key.clone(),
                    value: new.clone(),
                },
                AppliedChange::LinkSet { slot, new, .. } => StackWrite::SetLink {
                    slot: slot.clone(),
                    value: new.clone(),
                },
                AppliedChange::LinkAdded { slot, target } => StackWrite::AddLink {
                    slot: slot.clone(),
                    target: target.clone(),
                },
                AppliedChange::LinkRemoved { slot, .. } => {
                    StackWrite::UnsetLink { slot: slot.clone() }
                }
            },
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
                AppliedChange::Argument { .. } => {}
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
    copies: &[ComposedCopy],
) -> Result<PeppyLauncher, CompositionError> {
    let mut flat = flat_document(launcher, bare.deployments.clone(), bare.core_nodes.clone());
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
                    entry.write.render()
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
                    if *rendered != write.render() {
                        return Err(conflict(first, rendered.clone()));
                    }
                }
                (Some(Claim::Appended { .. }), StackWrite::AddLink { .. }) => {}
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
                    StackWrite::AddLink { .. } => Claim::Appended {
                        copy: copy.name.clone(),
                    },
                    write => Claim::Value {
                        copy: copy.name.clone(),
                        rendered: write.render(),
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
    write: &StackWrite,
    origin: &str,
) -> Result<(), CompositionError> {
    match write {
        StackWrite::Argument { key, value } => {
            instance.arguments.insert(key.clone(), value.clone());
        }
        StackWrite::SetLink { slot, value } => {
            instance.links.insert(slot.clone(), value.clone());
        }
        StackWrite::UnsetLink { slot } => {
            instance.links.remove(slot);
        }
        StackWrite::AddLink { slot, target } => {
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
    }
    Ok(())
}

/// The running stack with one more copy: every field the copy writes on
/// a stack instance already holds that value, except a slot the instance
/// declares vacant, which the copy's relays pair into.
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
            StackWrite::Argument { key, value } => {
                let old = running.arguments.get(key);
                if old != Some(value) {
                    return Err(refusal(format!(
                        "{field}: {} -> {}",
                        render_option(old),
                        render(value)
                    )));
                }
            }
            StackWrite::SetLink { slot, value } => {
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
            StackWrite::UnsetLink { slot } => match running.links.get(slot) {
                None => {}
                Some(LinkValue::Vacant(_)) => {
                    running.links.remove(slot);
                }
                Some(bound) => {
                    return Err(refusal(format!("{field}: {} -> (absent)", render(bound))));
                }
            },
            StackWrite::AddLink { slot, target } => {
                let bound = match running.links.get(slot) {
                    Some(LinkValue::Bound(Selection::Array(existing))) => {
                        existing.as_slice().contains(target)
                    }
                    _ => false,
                };
                if !bound {
                    return Err(refusal(format!("{field}: + {target}")));
                }
            }
        }
    }
    add_copy(&mut flat, copy)?;
    validate_flat(&flat)
}

/// The running stack without one copy: its instances and its placement
/// link gone, everything it wrote to the stack left as it runs. A stack
/// instance linking to one of the copy's instances keeps the copy.
pub(super) fn detach(
    existing: &PeppyLauncher,
    copy: &CopyRecord,
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
    validate_flat(&remaining)
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
) -> PeppyLauncher {
    PeppyLauncher {
        peppy_schema: launcher.peppy_schema,
        core_nodes,
        deployments,
        option_deployments: Vec::new(),
        components: Vec::new(),
        adjustments: Vec::new(),
        constraints: Vec::new(),
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
