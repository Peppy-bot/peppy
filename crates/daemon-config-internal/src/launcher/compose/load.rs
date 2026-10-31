//! Loading a composition: every fragment the launcher's options reference,
//! and the axes those fragments declare, read once and held to the
//! selection-independent checks.

use super::super::composition::{
    ComponentAxis, CopySettings, Fragment, LauncherFragmentParser, OptionDeployment, OptionSpec,
    OriginatedAdjustment, SelectionCondition, option_origin, validate_fragment_references,
    validate_guard,
};
use super::super::repository_paths::{LauncherPaths, PathRefusal, resolve_repository_path};
use super::super::types::PeppyLauncher;
use super::adjustments::{CopyAxis, LauncherAdjustment, Routed, ids_beside, route};
use super::constraints::{constraint_names_axis, names_axis};
use super::error::{AdjustmentTargetAmbiguous, CompositionError, CopyAdjustmentTargetUndefined};
use super::select::{CopyOrigin, UnitSelection, resolve_copy};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// One fragment, loaded and paired with the label the resolve report and
/// the error messages name it by.
#[derive(Debug, Clone)]
pub(super) struct LoadedFragment {
    /// The identity the checks key on: one per file, however many options
    /// name it, and one per inline body.
    pub(super) id: usize,
    pub(super) body: Fragment,
    /// The path relative to the launcher's directory for a file, `inline
    /// option `axis.option`` for a body written where the option is.
    pub(super) origin: String,
    /// The directory a file fragment's own fragment paths resolve against.
    /// `None` for an inline fragment, whose paths resolve where the
    /// declaring document's do.
    directory: Option<PathBuf>,
}

/// One option of one axis, with everything selecting it pulls in: its
/// fragments, the axes those fragments declare, the options they deploy on
/// them, and the fragments of every option of those axes.
#[derive(Debug, Clone)]
pub(super) struct LoadedOption {
    pub(super) name: String,
    pub(super) fragments: Vec<LoadedFragment>,
    pub(super) axes: Vec<ComponentAxis>,
    /// The option each of `axes` deploys unless a copy's `with` or a
    /// `--with` word says otherwise.
    pub(super) deployed: BTreeMap<String, String>,
    /// The fragments of each option of each of `axes`.
    nested: BTreeMap<String, BTreeMap<String, Vec<LoadedFragment>>>,
}

impl LoadedOption {
    pub(super) fn axis(&self, name: &str) -> Option<&ComponentAxis> {
        self.axes.iter().find(|axis| axis.name == name)
    }

    /// The fragments of one of this option's own axes' options.
    fn nested_fragments(&self, axis: &str, option: &str) -> &[LoadedFragment] {
        self.nested
            .get(axis)
            .and_then(|options| options.get(option))
            .map_or(&[], Vec::as_slice)
    }

    /// The fragments one run of this option pulls in under `own`, its
    /// selection, each with the own axis whose option brings it: its own
    /// fragments first, with no axis, then for each of its axes in
    /// declaration order the fragments of the option `own` picks there.
    fn selected_fragments<'a, 's>(
        &'a self,
        own: &'s UnitSelection,
    ) -> impl Iterator<Item = (Option<&'a str>, &'a LoadedFragment)> + use<'a, 's> {
        self.fragments
            .iter()
            .map(|fragment| (None, fragment))
            .chain(self.axes.iter().flat_map(move |axis| {
                own.option_of(&axis.name)
                    .map_or(&[][..], |option| self.nested_fragments(&axis.name, option))
                    .iter()
                    .map(move |fragment| (Some(axis.name.as_str()), fragment))
            }))
    }

    /// The fragments one run of this option pulls in under `own`, its
    /// selection: its own fragments, then those of the options `own`
    /// picks on its axes.
    pub(super) fn fragments_for(&self, own: &UnitSelection) -> Vec<&LoadedFragment> {
        self.selected_fragments(own)
            .map(|(_, fragment)| fragment)
            .collect()
    }

    /// The own axis that brings each instance one run of this option
    /// defines under `own`, or `None` for an instance of the option's own
    /// fragments, its base. Composition defines each id once, so each id
    /// comes from one fragment.
    pub(super) fn own_axis_by_id(&self, own: &UnitSelection) -> HashMap<&str, Option<&str>> {
        self.selected_fragments(own)
            .flat_map(|(axis, fragment)| {
                node_ids(std::iter::once(fragment)).map(move |id| (id, axis))
            })
            .collect()
    }

    /// Whether `option` of the own axis `axis` defines at least one
    /// instance: the part a word with a machine places. A nested fragment's
    /// deployments carry no condition, so the answer needs no composition.
    pub(super) fn brings_instances(&self, axis: &str, option: &str) -> bool {
        node_ids(self.nested_fragments(axis, option).iter())
            .next()
            .is_some()
    }

    /// The instance ids this option's own fragments define.
    fn own_ids(&self) -> impl Iterator<Item = &str> {
        node_ids(self.fragments.iter())
    }

    /// Every instance id this option can define, its own axes' options
    /// included, once for each fragment that defines it.
    fn iter_definable_ids(&self) -> impl Iterator<Item = &str> {
        node_ids(self.all_fragments())
    }

    /// Every instance id this option can define, its own axes' options
    /// included.
    pub(super) fn definable_ids(&self) -> HashSet<&str> {
        self.iter_definable_ids().collect()
    }

    /// Whether this option, its own axes' options included, can define the
    /// instance `id`. Answers without the set [`Self::definable_ids`]
    /// builds, for the callers that ask about one id.
    pub(super) fn can_define(&self, id: &str) -> bool {
        self.iter_definable_ids().any(|defined| defined == id)
    }

    pub(super) fn all_fragments(&self) -> impl Iterator<Item = &LoadedFragment> {
        self.fragments.iter().chain(
            self.nested
                .values()
                .flat_map(|options| options.values())
                .flat_map(|fragments| fragments.iter()),
        )
    }
}

pub(super) fn node_ids<'a>(
    fragments: impl Iterator<Item = &'a LoadedFragment>,
) -> impl Iterator<Item = &'a str> {
    fragments
        .flat_map(|fragment| fragment.body.deployments.iter())
        .flat_map(|deployment| deployment.instances.iter())
        .map(|instance| instance.instance_id.as_str())
}

/// Every option of every axis, loaded. Loading every option, selected or
/// not, is what lets a launch refuse what `repo index --check` refuses (an
/// option that does not provide its axis's interface, a guard naming an
/// unknown axis) before any selection is composed.
#[derive(Debug, Clone)]
pub(super) struct LoadedComposition {
    options: BTreeMap<String, BTreeMap<String, LoadedOption>>,
}

impl LoadedComposition {
    /// The loaded option of a launcher axis. Every option of every axis was
    /// loaded, so a selection resolved against the launcher always finds
    /// its option here.
    pub(super) fn option(&self, axis: &str, option: &str) -> &LoadedOption {
        self.options
            .get(axis)
            .and_then(|options| options.get(option))
            .unwrap_or_else(|| panic!("option `{axis}.{option}` was loaded with the launcher"))
    }

    pub(super) fn options_of(&self, axis: &str) -> impl Iterator<Item = (&String, &LoadedOption)> {
        self.options.get(axis).into_iter().flatten()
    }

    /// The fragments the stack runs under `selection`: those of every
    /// launcher axis's selected option, and of the options selected on
    /// their own axes.
    pub(super) fn stack_fragments(&self, selection: &UnitSelection) -> Vec<&LoadedFragment> {
        selection
            .entries
            .iter()
            .filter_map(|entry| {
                let option = entry.option.as_deref()?;
                self.options.get(&entry.axis)?.get(option)
            })
            .flat_map(|loaded| loaded.fragments_for(selection))
            .collect()
    }
}

/// The label error messages and reports name a launcher by: its file name,
/// or the path as written when it has no final component.
pub(super) fn launcher_file_label(launcher_file: &Path) -> String {
    launcher_file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| launcher_file.display().to_string())
}

/// Reads every fragment the launcher references and runs the
/// selection-independent checks: path safety, fragment parse, nesting depth,
/// axes declared once in reach, `provides` satisfaction, guard and
/// constraint references, adjustment targets defined beside their entry,
/// and copies that select what their option declares.
pub(super) fn load_composition(
    launcher: &PeppyLauncher,
    launcher_file: &Path,
) -> Result<LoadedComposition, CompositionError> {
    let launcher_label = launcher_file_label(launcher_file);
    let mut reader = FragmentReader {
        launcher_file,
        paths: None,
        files: HashMap::new(),
        next_id: 0,
    };
    let mut options: BTreeMap<String, BTreeMap<String, LoadedOption>> = BTreeMap::new();
    for axis in &launcher.components {
        let mut loaded_axis = BTreeMap::new();
        for (option_name, spec) in &axis.options {
            let origin = format!(
                "{launcher_label}, {}",
                option_origin(&axis.name, option_name)
            );
            let fragments = reader.read_parts(spec, &origin, None, &axis.name, option_name)?;
            let loaded = load_option(&mut reader, launcher, option_name, fragments)?;
            loaded_axis.insert(option_name.clone(), loaded);
        }
        options.insert(axis.name.clone(), loaded_axis);
    }
    let loaded = LoadedComposition { options };
    let (copy_axes, routed) = route(launcher, &loaded, &launcher_label);
    let adjustments: Vec<&LauncherAdjustment<'_>> = routed.iter().map(|r| &r.entry).collect();
    check_provides(launcher, &loaded)?;
    check_references(launcher, &loaded, &adjustments)?;
    check_targets(launcher, &loaded, &adjustments)?;
    check_copy_axis_references(launcher, &loaded, &copy_axes, &routed)?;
    check_file_copies(launcher, &loaded)?;
    Ok(loaded)
}

/// One option's fragments folded into what selecting it pulls in. Its axes
/// are declared once among its fragments, under names distinct from the
/// launcher's, so a guard naming one names exactly one axis.
fn load_option(
    reader: &mut FragmentReader<'_>,
    launcher: &PeppyLauncher,
    name: &str,
    fragments: Vec<LoadedFragment>,
) -> Result<LoadedOption, CompositionError> {
    let mut axes: Vec<ComponentAxis> = Vec::new();
    let mut axis_origin: HashMap<String, String> = HashMap::new();
    let mut deployed: BTreeMap<String, String> = BTreeMap::new();
    let mut deployed_origin: HashMap<String, String> = HashMap::new();
    for fragment in &fragments {
        for axis in &fragment.body.components {
            if launcher.components.iter().any(|own| own.name == axis.name) {
                return Err(CompositionError::AxisInReachTwice {
                    axis: axis.name.clone(),
                    first: String::from("the launcher"),
                    second: fragment.origin.clone(),
                });
            }
            if let Some(first) = axis_origin.insert(axis.name.clone(), fragment.origin.clone()) {
                return Err(CompositionError::AxisDeclaredTwice {
                    option: name.to_owned(),
                    axis: axis.name.clone(),
                    first,
                    second: fragment.origin.clone(),
                });
            }
            axes.push(axis.clone());
        }
    }
    for fragment in &fragments {
        for entry in &fragment.body.option_deployments {
            if let Some(first) = deployed_origin.insert(entry.axis.clone(), fragment.origin.clone())
            {
                return Err(CompositionError::AxisDeployedTwice {
                    option: name.to_owned(),
                    axis: entry.axis.clone(),
                    first,
                    second: fragment.origin.clone(),
                });
            }
            deployed.insert(entry.axis.clone(), entry.option.clone());
        }
    }
    let mut nested: BTreeMap<String, BTreeMap<String, Vec<LoadedFragment>>> = BTreeMap::new();
    for fragment in &fragments {
        for axis in &fragment.body.components {
            let mut loaded_axis = BTreeMap::new();
            for (option_name, spec) in &axis.options {
                let nested_origin = format!(
                    "{}, {}",
                    fragment.origin,
                    option_origin(&axis.name, option_name)
                );
                let parts = reader.read_parts(
                    spec,
                    &nested_origin,
                    fragment.directory.as_deref(),
                    &axis.name,
                    option_name,
                )?;
                for part in &parts {
                    if !part.body.components.is_empty() {
                        return Err(CompositionError::NestedComponents {
                            path: part.origin.clone(),
                            origin: nested_origin.clone(),
                        });
                    }
                }
                loaded_axis.insert(option_name.clone(), parts);
            }
            nested.insert(axis.name.clone(), loaded_axis);
        }
    }
    Ok(LoadedOption {
        name: name.to_owned(),
        fragments,
        axes,
        deployed,
        nested,
    })
}

/// The option of one of `axes`, other than an option of `excluded_axis`,
/// whose instances `target` names, as `(axis, option)`.
fn option_defining<'a>(
    axes: impl Iterator<Item = &'a ComponentAxis>,
    loaded: &'a LoadedComposition,
    target: &str,
    excluded_axis: Option<&str>,
) -> Option<(&'a str, &'a str)> {
    axes.filter(|axis| excluded_axis != Some(axis.name.as_str()))
        .flat_map(|axis| {
            loaded
                .options_of(&axis.name)
                .map(move |(option, loaded)| (axis.name.as_str(), option.as_str(), loaded))
        })
        .find(|(_, _, option)| option.can_define(target))
        .map(|(axis, option, _)| (axis, option))
}

/// A stack option, other than the entry's own or a sibling of it, whose
/// instances the entry's target names: with the entry's own option the
/// target is that option's instance, and a sibling never runs beside it.
fn stack_option_defining<'a>(
    launcher: &'a PeppyLauncher,
    loaded: &'a LoadedComposition,
    entry: &LauncherAdjustment<'_>,
) -> Option<(&'a str, &'a str)> {
    option_defining(
        launcher.stack_axes(),
        loaded,
        entry.adjustment.target.as_str(),
        entry.under.map(|(axis, _)| axis),
    )
}

/// A copy option whose instances `target` names.
fn copy_option_defining<'a>(
    launcher: &'a PeppyLauncher,
    loaded: &'a LoadedComposition,
    target: &str,
) -> Option<(&'a str, &'a str)> {
    option_defining(launcher.repeatable_axes(), loaded, target, None)
}

/// Every option defines its axis's interface, whatever else it privately
/// defines: that is the promise a link or adjustment against a `provides`
/// id rests on. A fragment's own axes promise the same of their options.
fn check_provides(
    launcher: &PeppyLauncher,
    loaded: &LoadedComposition,
) -> Result<(), CompositionError> {
    for axis in &launcher.components {
        for (option_name, option) in loaded.options_of(&axis.name) {
            let defined: HashSet<&str> = option.own_ids().collect();
            check_provided(axis, option_name, &defined)?;
            for own_axis in &option.axes {
                for nested_option in own_axis.options.keys() {
                    let defined: HashSet<&str> = node_ids(
                        option
                            .nested_fragments(&own_axis.name, nested_option)
                            .iter(),
                    )
                    .collect();
                    check_provided(own_axis, nested_option, &defined)?;
                }
            }
        }
    }
    Ok(())
}

fn check_provided(
    axis: &ComponentAxis,
    option: &str,
    defined: &HashSet<&str>,
) -> Result<(), CompositionError> {
    for id in &axis.provides {
        if !defined.contains(id.as_str()) {
            return Err(CompositionError::ProvidesUnmet {
                axis: axis.name.clone(),
                option: option.to_owned(),
                id: id.to_string(),
            });
        }
    }
    Ok(())
}

/// Every guard and constraint of every fragment, and every guard on an
/// option's adjustments, names axes in its reach: the launcher's, and the
/// axes of the option it belongs to.
fn check_references(
    launcher: &PeppyLauncher,
    loaded: &LoadedComposition,
    adjustments: &[&LauncherAdjustment<'_>],
) -> Result<(), CompositionError> {
    for axis in &launcher.components {
        for (option_name, option) in loaded.options_of(&axis.name) {
            let in_reach: Vec<&ComponentAxis> = launcher
                .components
                .iter()
                .chain(option.axes.iter())
                .collect();
            let origin = option_origin(&axis.name, option_name);
            for fragment in option.all_fragments() {
                validate_fragment_references(&fragment.body, &in_reach, &fragment.origin).map_err(
                    |detail| CompositionError::FragmentReferencesUnknownAxis {
                        path: fragment.origin.clone(),
                        origin: origin.clone(),
                        detail,
                    },
                )?;
            }
            let under_this_option = adjustments
                .iter()
                .filter(|entry| entry.under == Some((axis.name.as_str(), option_name.as_str())));
            for entry in under_this_option {
                if let Some(when) = &entry.adjustment.when {
                    validate_guard(when, &in_reach, &entry.origin)
                        .map_err(|detail| CompositionError::AdjustmentGuard { detail })?;
                }
            }
        }
    }
    Ok(())
}

/// What a refusal over a target offers: the ids in reach, or that there are
/// none.
fn in_reach<'a>(ids: impl IntoIterator<Item = &'a str>) -> String {
    let listed = crate::error::format_quoted_list(ids.into_iter().collect::<BTreeSet<_>>());
    if listed.is_empty() {
        String::from("no instance runs beside it to target")
    } else {
        format!("target one of {listed}")
    }
}

/// A launcher adjustment target the selection does not define is skipped; a
/// target nothing in the entry's reach can define is a dead reference, refused
/// here once for every selection. A fragment's own entries name the
/// instances that fragment deploys, checked where the fragment is read.
fn check_targets(
    launcher: &PeppyLauncher,
    loaded: &LoadedComposition,
    adjustments: &[&LauncherAdjustment<'_>],
) -> Result<(), CompositionError> {
    for LauncherAdjustment {
        adjustment,
        origin,
        under,
    } in adjustments
    {
        let own_copy_axis = under.map(|(axis, _)| axis).filter(|axis| {
            launcher
                .repeatable_axes()
                .any(|copy_axis| copy_axis.name == *axis)
        });
        let axes = launcher.components.iter().filter(|axis| {
            !axis.cardinality.is_repeatable() || own_copy_axis.is_none_or(|own| own == axis.name)
        });
        let beside = ids_beside(launcher, loaded, *under, axes);
        if !beside.contains(adjustment.target.as_str()) {
            return Err(CompositionError::TargetDefinedNowhere {
                target: adjustment.target.to_string(),
                origin: origin.clone(),
                in_reach: in_reach(beside.iter().copied()),
            });
        }
    }
    Ok(())
}

/// A unit's selection fills the launcher's stack axes and, for a copy, its
/// own axis: a fragment's guard or constraint naming another copy axis can
/// never hold, a launcher constraint or adjustment naming two copy axes
/// holds for neither, an adjustment that runs in copies writes an instance those
/// copies or the stack define and cannot read a stack option's own axes,
/// and a copy's fragment has no core node link to declare, its name being
/// its placement. Each is refused where it is written.
fn check_copy_axis_references(
    launcher: &PeppyLauncher,
    loaded: &LoadedComposition,
    copy_axes: &[CopyAxis<'_>],
    routed: &[Routed<'_>],
) -> Result<(), CompositionError> {
    check_fragments_copy_axis_references(launcher, loaded)?;
    check_launcher_constraints_copy_axes(launcher)?;
    for routed in routed {
        check_routed_adjustment_copy_axes(launcher, loaded, copy_axes, routed)?;
    }
    Ok(())
}

/// Every fragment of every option, held to the copy axes its unit can
/// fill: see [`check_fragment_copy_axis_references`].
fn check_fragments_copy_axis_references(
    launcher: &PeppyLauncher,
    loaded: &LoadedComposition,
) -> Result<(), CompositionError> {
    for axis in &launcher.components {
        let foreign: Vec<&ComponentAxis> = launcher
            .repeatable_axes()
            .filter(|copy_axis| copy_axis.name != axis.name)
            .collect();
        for (_, option) in loaded.options_of(&axis.name) {
            for fragment in option.all_fragments() {
                check_fragment_copy_axis_references(axis, fragment, &foreign)?;
            }
        }
    }
    Ok(())
}

/// A fragment of an option of `axis`: a copy's fragment declares no core
/// node link, and no guard or constraint of the fragment names one of the
/// `foreign` copy axes, which its unit never fills.
fn check_fragment_copy_axis_references(
    axis: &ComponentAxis,
    fragment: &LoadedFragment,
    foreign: &[&ComponentAxis],
) -> Result<(), CompositionError> {
    if axis.cardinality.is_repeatable() && !fragment.body.core_nodes.is_empty() {
        return Err(CompositionError::CopyFragmentCoreNodes {
            origin: fragment.origin.clone(),
            axis: axis.name.clone(),
        });
    }
    for adjustment in &fragment.body.adjustments {
        if let Some(copy_axis) = foreign
            .iter()
            .find(|copy_axis| names_axis(adjustment.when.as_ref(), &copy_axis.name))
        {
            return Err(CompositionError::GuardOnCopyAxis {
                target: adjustment.target.to_string(),
                origin: fragment.origin.clone(),
                axis: copy_axis.name.clone(),
            });
        }
    }
    for constraint in &fragment.body.constraints {
        if let Some(copy_axis) = foreign
            .iter()
            .find(|copy_axis| constraint_names_axis(constraint, &copy_axis.name))
        {
            return Err(CompositionError::ConstraintOnCopyAxis {
                origin: fragment.origin.clone(),
                axis: copy_axis.name.clone(),
            });
        }
    }
    Ok(())
}

/// A launcher constraint naming two copy axes holds for neither.
fn check_launcher_constraints_copy_axes(launcher: &PeppyLauncher) -> Result<(), CompositionError> {
    for (position, constraint) in launcher.constraints.iter().enumerate() {
        let named: Vec<&str> = launcher
            .repeatable_axes()
            .filter(|copy_axis| constraint_names_axis(constraint, &copy_axis.name))
            .map(|copy_axis| copy_axis.name.as_str())
            .collect();
        if named.len() > 1 {
            return Err(CompositionError::ConstraintSpansCopyAxes {
                position: position + 1,
                axes: crate::error::format_quoted_list(named),
            });
        }
    }
    Ok(())
}

/// One launcher adjustment, routed. It names at most one copy axis. An
/// entry naming none cannot target an id both a stack option and a copy
/// option define, unless its own option defines the id. In each axis of
/// copies that runs it, it writes an instance those copies or the stack
/// define, and an entry from outside the copy axes reads no stack
/// option's own axis.
fn check_routed_adjustment_copy_axes(
    launcher: &PeppyLauncher,
    loaded: &LoadedComposition,
    copy_axes: &[CopyAxis<'_>],
    routed: &Routed<'_>,
) -> Result<(), CompositionError> {
    let entry = &routed.entry;
    let named: Vec<&str> = copy_axes
        .iter()
        .filter(|copy_axis| entry.names_axis(copy_axis.name))
        .map(|copy_axis| copy_axis.name)
        .collect();
    if named.len() > 1 {
        return Err(CompositionError::AdjustmentSpansCopyAxes {
            target: entry.adjustment.target.to_string(),
            origin: entry.origin.clone(),
            axes: crate::error::format_quoted_list(named),
        });
    }
    let target = entry.adjustment.target.as_str();
    // An entry writing an instance its own option defines is that option's
    // write, whichever other options define the id.
    let own_write = entry
        .under
        .is_some_and(|(axis, option)| loaded.option(axis, option).can_define(target));
    if !own_write
        && named.is_empty()
        && let Some((stack_axis, stack_option)) = stack_option_defining(launcher, loaded, entry)
        && let Some((copy_axis, copy_option)) = copy_option_defining(launcher, loaded, target)
    {
        return Err(CompositionError::AdjustmentTargetAmbiguous(Box::new(
            AdjustmentTargetAmbiguous {
                target: target.to_owned(),
                origin: entry.origin.clone(),
                stack: format!("{stack_axis}.{stack_option}"),
                keeping: entry.under.map_or_else(String::new, |(axis, option)| {
                    format!(
                        ", guarded `when: {{ {axis}: \"{option}\" }}` as option `{axis}.{option}` guards it now"
                    )
                }),
                copy_axis: copy_axis.to_owned(),
                copy_option: copy_option.to_owned(),
            },
        )));
    }
    // An entry under an option that runs as copies is selected in that
    // axis's copies alone; one from an option that fills once or the
    // top level runs in every axis of copies it reaches.
    let under_copy_axis = entry
        .under
        .is_some_and(|(under, _)| copy_axes.iter().any(|copy_axis| copy_axis.name == under));
    for copy_axis in copy_axes
        .iter()
        .filter(|copy_axis| routed.copy_axes.contains(&copy_axis.name))
    {
        check_copy_adjustment_target_defined(
            loaded,
            entry,
            copy_axis.name,
            &routed.stack_ids,
            target,
        )?;
        if under_copy_axis {
            continue;
        }
        check_copy_adjustment_reads_no_option_axis(launcher, entry, copy_axis.name, target)?;
    }
    Ok(())
}

/// An entry that speaks about `copy_axis` writes an instance of its
/// copies or of the stack.
fn check_copy_adjustment_target_defined(
    loaded: &LoadedComposition,
    entry: &LauncherAdjustment<'_>,
    copy_axis: &str,
    stack_ids: &HashSet<&str>,
    target: &str,
) -> Result<(), CompositionError> {
    if !entry.names_axis(copy_axis) || stack_ids.contains(target) {
        return Ok(());
    }
    let defined: HashSet<&str> = entry
        .options_named_on(copy_axis)
        .into_iter()
        .flat_map(|option| loaded.option(copy_axis, option).definable_ids())
        .collect();
    if defined.contains(target) {
        return Ok(());
    }
    let named = entry.options_named_on(copy_axis);
    let defining: Vec<&str> = loaded
        .options_of(copy_axis)
        .filter(|(option, loaded)| !named.contains(&option.as_str()) && loaded.can_define(target))
        .map(|(option, _)| option.as_str())
        .collect();
    let reach: Vec<&str> = defined.iter().chain(stack_ids.iter()).copied().collect();
    let joiner = if reach.is_empty() { "; " } else { ", or " };
    Err(CompositionError::CopyAdjustmentTargetUndefined(Box::new(
        CopyAdjustmentTargetUndefined {
            target: target.to_owned(),
            origin: entry.origin.clone(),
            copy_axis: copy_axis.to_owned(),
            named: crate::error::format_quoted_list(named),
            in_reach: in_reach(reach.iter().copied()),
            defining: match defining.as_slice() {
                [] => String::new(),
                [option] => format!("{joiner}guard it `when: {{ {copy_axis}: \"{option}\" }}`"),
                options => format!(
                    "{joiner}guard it `when: {{ {copy_axis}: [{}] }}`",
                    options
                        .iter()
                        .map(|option| format!("\"{option}\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            },
        },
    )))
}

/// A copy's selection holds the launcher's axes and the copied option's
/// own, so an entry that reaches the copies of `copy_axis` from an option
/// that fills once cannot be guarded on that option's axes.
fn check_copy_adjustment_reads_no_option_axis(
    launcher: &PeppyLauncher,
    entry: &LauncherAdjustment<'_>,
    copy_axis: &str,
    target: &str,
) -> Result<(), CompositionError> {
    let launcher_axis = |name: &str| launcher.components.iter().any(|axis| axis.name == name);
    let own_axis = entry
        .adjustment
        .when
        .iter()
        .flat_map(|when| when.keys())
        .find(|axis| !launcher_axis(axis));
    if let Some(axis) = own_axis {
        return Err(CompositionError::CopyAdjustmentReadsOptionAxis {
            target: target.to_owned(),
            origin: entry.origin.clone(),
            copy_axis: copy_axis.to_owned(),
            axis: axis.clone(),
        });
    }
    Ok(())
}

/// Every copy the launcher deploys selects axes its option declares, with
/// options those axes declare, and overrides arguments of instances its
/// selection defines.
fn check_file_copies(
    launcher: &PeppyLauncher,
    loaded: &LoadedComposition,
) -> Result<(), CompositionError> {
    for entry in &launcher.option_deployments {
        let OptionDeployment { axis, option, .. } = entry;
        let loaded_option = loaded.option(axis, option);
        // A copy's guards read the stack's axes and the copy's own; every
        // copy axis, its own included, is filled by the copy's name alone.
        let in_reach: Vec<&ComponentAxis> = launcher
            .stack_axes()
            .chain(loaded_option.axes.iter())
            .collect();
        let definable = loaded_option.definable_ids();
        for copy in &entry.instances {
            let CopySettings {
                with,
                arguments,
                adjustments,
            } = entry.settings_for(copy);
            let own = match resolve_copy(
                loaded_option,
                axis,
                copy.instance_id.as_str(),
                &with,
                CopyOrigin::File,
            ) {
                Ok(own) => Some(own),
                // A `one` axis the file leaves for a launch word: which
                // fragments the copy runs is known at launch.
                Err(CompositionError::UnresolvedCopyAxis(_)) => None,
                Err(error) => return Err(error),
            };
            let defined: BTreeSet<&str> = match &own {
                Some(own) => node_ids(loaded_option.fragments_for(own).into_iter()).collect(),
                None => definable.iter().copied().collect(),
            };
            for OriginatedAdjustment { adjustment, origin } in &adjustments {
                if let Some(when) = &adjustment.when {
                    check_copy_guard(launcher, when, &in_reach, origin)?;
                }
                // An unguarded adjustment writes what the copy runs; a
                // guarded one may await an option another selection brings.
                let target = adjustment.target.as_str();
                let is_reachable = match &adjustment.when {
                    Some(_) => definable.contains(target),
                    None => defined.contains(target),
                };
                if !is_reachable {
                    return Err(CompositionError::CopyAdjustmentTarget {
                        origin: origin.clone(),
                        target: adjustment.target.to_string(),
                        available: crate::error::format_quoted_list(defined.iter().copied()),
                    });
                }
            }
            for target in arguments.keys() {
                if !defined.contains(target.as_str()) {
                    return Err(CompositionError::ArgumentTargetAbsent {
                        copy: copy.instance_id.to_string(),
                        target: target.clone(),
                        available: crate::error::format_quoted_list(defined.iter().copied()),
                    });
                }
            }
        }
    }
    Ok(())
}

/// A file copy's adjustment guard reads the stack's axes and the copy's
/// own. A guard naming a copy axis is refused, since the copy's name
/// alone fills every copy axis.
fn check_copy_guard(
    launcher: &PeppyLauncher,
    when: &SelectionCondition,
    in_reach: &[&ComponentAxis],
    origin: &str,
) -> Result<(), CompositionError> {
    if let Some(copy_axis) = launcher
        .repeatable_axes()
        .find(|copy_axis| names_axis(Some(when), &copy_axis.name))
    {
        return Err(CompositionError::CopyAdjustmentOnCopyAxis {
            origin: origin.to_owned(),
            axis: copy_axis.name.clone(),
        });
    }
    validate_guard(when, in_reach, origin)
        .map_err(|detail| CompositionError::AdjustmentGuard { detail })
}

/// Reads fragment files once each, resolving every path inside the
/// launcher's repository, giving each file one identity however many
/// options name it and each inline body its own.
struct FragmentReader<'a> {
    launcher_file: &'a Path,
    // Resolved at the first file reference. Inline-only compositions
    // perform no filesystem reads.
    paths: Option<LauncherPaths>,
    // Several options may reference one fragment file (the relay fragment
    // under both simulated robots); it is read and parsed once, keyed by
    // its resolved path, and keeps the identity it was given.
    files: HashMap<PathBuf, (usize, Fragment)>,
    next_id: usize,
}

impl FragmentReader<'_> {
    /// One option's parts: its files, read relative to `directory` (the
    /// declaring fragment's own, or the launcher's when `None`), then the
    /// body written where the option is.
    fn read_parts(
        &mut self,
        spec: &OptionSpec,
        origin: &str,
        directory: Option<&Path>,
        axis: &str,
        option: &str,
    ) -> Result<Vec<LoadedFragment>, CompositionError> {
        let mut loaded = Vec::new();
        for raw in &spec.files {
            let (id, body, path) = self.read_file(raw, origin, directory)?;
            loaded.push(LoadedFragment {
                id,
                body,
                origin: self.label_of(&path),
                directory: path.parent().map(Path::to_path_buf),
            });
        }
        if let Some(body) = &spec.body {
            loaded.push(LoadedFragment {
                id: self.fresh_id(),
                body: body.clone(),
                origin: format!("inline option `{axis}.{option}`"),
                directory: directory.map(Path::to_path_buf),
            });
        }
        Ok(loaded)
    }

    fn fresh_id(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn read_file(
        &mut self,
        raw: &str,
        origin: &str,
        directory: Option<&Path>,
    ) -> Result<(usize, Fragment, PathBuf), CompositionError> {
        if self.paths.is_none() {
            let paths = LauncherPaths::of_launcher(self.launcher_file)
                .map_err(|reason| CompositionError::LauncherDirectory { reason })?;
            self.paths = Some(paths);
        }
        let paths = self.paths.as_ref().expect("resolved just above");
        let base = directory.unwrap_or(&paths.directory);
        let path = resolve_fragment_path(paths, base, raw, origin)?;
        if let Some((id, cached)) = self.files.get(&path) {
            return Ok((*id, cached.clone(), path));
        }
        let body = read_fragment_file(&path, paths, raw, origin)?;
        let id = self.fresh_id();
        self.files.insert(path.clone(), (id, body.clone()));
        Ok((id, body, path))
    }

    /// A file's label: its path relative to the launcher's directory.
    fn label_of(&self, path: &Path) -> String {
        let directory = &self
            .paths
            .as_ref()
            .expect("a file was read, so the paths are resolved")
            .directory;
        relative_path(directory, path)
    }
}

/// `path` relative to `base`, both canonical: `fragments/web.json5`, or
/// `../simulation/fragments/mujoco.json5` for a sibling directory.
fn relative_path(base: &Path, path: &Path) -> String {
    let shared = base
        .components()
        .zip(path.components())
        .take_while(|(a, b)| a == b)
        .count();
    let ups = base.components().count() - shared;
    let mut relative = PathBuf::new();
    for _ in 0..ups {
        relative.push("..");
    }
    for component in path.components().skip(shared) {
        relative.push(component);
    }
    relative.to_string_lossy().into_owned()
}

/// Reads and parses one `launcher_fragment/v1` file through the shared
/// fragment parser (which owns the read-and-refuse-empty half), its includes
/// resolved inside the launcher's repository.
fn read_fragment_file(
    path: &Path,
    paths: &LauncherPaths,
    raw: &str,
    origin: &str,
) -> Result<Fragment, CompositionError> {
    let parsed =
        LauncherFragmentParser::from_launcher_fragment(path, paths).map_err(|e| match &e {
            crate::error::Error::Parsing(
                crate::error::ParsingError::CannotRead(..)
                | crate::error::ParsingError::EmptyContent(..),
            ) => CompositionError::FragmentUnreadable {
                path: raw.to_owned(),
                origin: origin.to_owned(),
                detail: e.to_string(),
            },
            other => CompositionError::FragmentInvalid {
                path: raw.to_owned(),
                origin: origin.to_owned(),
                detail: other.to_string(),
            },
        })?;
    Ok(parsed.body)
}

/// The canonical file of the fragment path `raw` that `origin` writes in a
/// document of `directory`, under the path rules of
/// [`resolve_repository_path`].
fn resolve_fragment_path(
    paths: &LauncherPaths,
    directory: &Path,
    raw: &str,
    origin: &str,
) -> Result<PathBuf, CompositionError> {
    resolve_repository_path(paths, directory, raw).map_err(|refusal| match refusal {
        PathRefusal::Rule(reason) => CompositionError::FragmentPath {
            path: raw.to_owned(),
            origin: origin.to_owned(),
            reason: reason.to_owned(),
        },
        PathRefusal::Unresolved(error) => CompositionError::FragmentUnreadable {
            path: raw.to_owned(),
            origin: origin.to_owned(),
            detail: error.to_string(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fragment_label_is_relative_to_the_launcher_directory() {
        let base = Path::new("/repo/openarm");
        assert_eq!(
            relative_path(base, Path::new("/repo/openarm/fragments/web.json5")),
            "fragments/web.json5"
        );
        assert_eq!(
            relative_path(base, Path::new("/repo/simulation/fragments/mujoco.json5")),
            "../simulation/fragments/mujoco.json5"
        );
    }
}
