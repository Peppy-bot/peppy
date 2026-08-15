//! Selection resolution and flattening: turning a composed launcher plus a
//! `--with` selection into the ordinary flat launcher the rest of the
//! pipeline consumes.
//!
//! Composition is a pure function of the launcher, its fragments, and the
//! selection; the only I/O is reading fragment files, all of them resolved
//! relative to the launcher's own directory so a repository launcher and a
//! filesystem launcher compose identically.

use super::composition::{
    Adjustment, ComponentAxis, FragmentPart, LauncherFragmentParser, validate_adjustment,
    validate_guard,
};
use super::parse::PeppyLauncherParser;
use super::types::{Deployment, LinkTargets, LinkValue, PeppyLauncher, Selection};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component as PathComponent, Path, PathBuf};
use thiserror::Error;

/// Everything composition can refuse: bad `--with` words, unreadable or
/// unsafe fragment paths, options that do not provide what their axis
/// promises, and adjustments that fight.
///
/// The `--with` refusals are launch refusals (the words are caller input);
/// the rest surface wherever the launcher or its fragments are read, which
/// for `repo index --check` is authoring time and for a launch is before
/// anything is pinned or started.
#[derive(Debug, Error)]
pub enum CompositionError {
    #[error(
        "`--with` was given but this launcher declares no `components`; it is a flat stack with \
         nothing to select"
    )]
    WithOnFlatLauncher,

    #[error("`--with {word}` names no axis or option of this launcher.{menu}")]
    UnknownSelection { word: String, menu: String },

    #[error(
        "`--with {word}` matches an option of more than one axis ({axes}); say which you mean \
         with the `axis=option` form"
    )]
    AmbiguousSelection { word: String, axes: String },

    #[error(
        "axis `{axis}` is selected twice with different options (`{first}`, `{second}`); an axis \
         takes one option, never two"
    )]
    ConflictingSelection {
        axis: String,
        first: String,
        second: String,
    },

    #[error(
        "axis `{axis}` is neither selected nor defaulted and is not `optional`; choose one of \
         its options with `--with`{options}"
    )]
    UnresolvedAxis { axis: String, options: String },

    #[error(
        "fragment path `{path}` in {origin} is not usable: {reason}. A fragment path is \
         relative to the launcher's own directory and stays inside it"
    )]
    FragmentPath {
        path: String,
        origin: String,
        reason: String,
    },

    #[error("fragment `{path}` referenced by {origin} cannot be read: {detail}")]
    FragmentUnreadable {
        path: String,
        origin: String,
        detail: String,
    },

    #[error("fragment `{path}` referenced by {origin} does not parse: {detail}")]
    FragmentInvalid {
        path: String,
        origin: String,
        detail: String,
    },

    #[error(
        "option `{option}` of axis `{axis}` does not define `{id}`, which the axis's `provides` \
         promises; every option defines the axis's interface ids"
    )]
    ProvidesUnmet {
        axis: String,
        option: String,
        id: String,
    },

    #[error(
        "adjustment on `{target}` in {origin} names a target no option of this launcher defines \
         anywhere; that is a dead reference or a typo"
    )]
    TargetDefinedNowhere {
        target: String,
        origin: String,
    },

    #[error(
        "instance `{id}` is defined by both {first} and {second}; each instance id belongs to \
         one origin"
    )]
    DuplicateInstanceId {
        id: String,
        first: String,
        second: String,
    },

    #[error(
        "two fragments adjust the same thing: {first} and {second} both write \
         `{target}.{field}`. Fragments refuse to fight over one value; the base specializes \
         fragments, not the other way around"
    )]
    AdjustmentsConflict {
        target: String,
        field: String,
        first: String,
        second: String,
    },

    #[error(
        "adjustment in {origin} cannot append to slot `{slot}` on `{target}`: the slot holds \
         {holds}. Appending is for array bindings; replacing is `set_links`'s job"
    )]
    AddLinksOnNonArray {
        origin: String,
        target: String,
        slot: String,
        holds: &'static str,
    },

    #[error(
        "adjustment in {origin} adds `{added}` to slot `{slot}` on `{target}`, which already \
         binds it: a slot's bound set lists each producer once"
    )]
    AddLinksDuplicateTarget {
        origin: String,
        target: String,
        slot: String,
        added: String,
    },

    #[error("the flattened launcher does not validate: {0}")]
    FlatValidation(#[from] crate::error::Error),
}

/// How one axis of a resolved selection came to be filled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionSource {
    /// Named by `--with`.
    Explicit,
    /// Filled from the axis's `default`.
    Default,
    /// An `optional` axis left unfilled; contributes nothing.
    Unfilled,
}

/// One axis's fate in a resolved selection.
#[derive(Debug, Clone)]
pub struct SelectionEntry {
    pub axis: String,
    pub option: Option<String>,
    pub source: SelectionSource,
}

/// A `--with` selection resolved against a launcher's axes: one entry per
/// axis, in declaration order.
#[derive(Debug, Clone, Default)]
pub struct ComponentSelection {
    pub entries: Vec<SelectionEntry>,
}

impl ComponentSelection {
    /// The option selected on `axis`, or `None` when the axis is unfilled.
    pub fn option_on(&self, axis: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|entry| entry.axis == axis)
            .and_then(|entry| entry.option.as_deref())
    }

    /// The one-line echo of the full resolution, as launch feedback prints
    /// it before anything runs: `robot=mujoco  commander=web (default)
    /// recorder=(off)`.
    pub fn echo(&self) -> String {
        self.entries
            .iter()
            .map(|entry| match (&entry.option, entry.source) {
                (Some(option), SelectionSource::Default) => format!("{}={option} (default)", entry.axis),
                (Some(option), _) => format!("{}={option}", entry.axis),
                (None, _) => format!("{}=(off)", entry.axis),
            })
            .collect::<Vec<_>>()
            .join("  ")
    }
}

/// The `--with` words against the axes: each entry is `option` or
/// `axis=option`. At most one option per axis (repeating the same option is
/// the same choice); unselected axes take their default, and a required axis
/// with neither refuses.
pub fn resolve_selection(
    axes: &[ComponentAxis],
    words: &[String],
) -> Result<ComponentSelection, CompositionError> {
    let menu = axes_menu(axes);
    let mut chosen: BTreeMap<String, String> = BTreeMap::new();

    for word in words {
        let (axis_name, option_name) = match word.split_once('=') {
            Some((axis, option)) => (Some(axis), option),
            None => (None, word.as_str()),
        };

        let axis_name = match axis_name {
            Some(explicit) => {
                if !axes.iter().any(|axis| axis.name == explicit) {
                    return Err(CompositionError::UnknownSelection {
                        word: word.clone(),
                        menu,
                    });
                }
                explicit
            }
            // A bare word resolves against option names across every axis;
            // matching more than one is ambiguous and asks for the explicit
            // form.
            None => {
                let holders: Vec<&str> = axes
                    .iter()
                    .filter(|axis| axis.options.contains_key(option_name))
                    .map(ComponentAxis::as_str)
                    .collect();
                match holders.as_slice() {
                    [] => {
                        return Err(CompositionError::UnknownSelection {
                            word: word.clone(),
                            menu,
                        })
                    }
                    [axis] => *axis,
                    many => {
                        return Err(CompositionError::AmbiguousSelection {
                            word: word.clone(),
                            axes: many
                                .iter()
                                .map(|axis| format!("`{axis}`"))
                                .collect::<Vec<_>>()
                                .join(", "),
                        })
                    }
                }
            }
        };

        let axis = axes
            .iter()
            .find(|axis| axis.name == axis_name)
            .expect("axis existence checked above");
        if !axis.options.contains_key(option_name) {
            return Err(CompositionError::UnknownSelection {
                word: word.clone(),
                menu,
            });
        }
        if let Some(first) = chosen.get(axis_name)
            && first != option_name
        {
            return Err(CompositionError::ConflictingSelection {
                axis: axis_name.to_owned(),
                first: first.clone(),
                second: option_name.to_owned(),
            });
        }
        chosen.insert(axis_name.to_owned(), option_name.to_owned());
    }

    let mut entries = Vec::with_capacity(axes.len());
    for axis in axes {
        match chosen.get(axis.name.as_str()) {
            Some(option) => entries.push(SelectionEntry {
                axis: axis.name.clone(),
                option: Some(option.clone()),
                source: SelectionSource::Explicit,
            }),
            None => match &axis.default {
                Some(default) => entries.push(SelectionEntry {
                    axis: axis.name.clone(),
                    option: Some(default.clone()),
                    source: SelectionSource::Default,
                }),
                None if axis.optional => entries.push(SelectionEntry {
                    axis: axis.name.clone(),
                    option: None,
                    source: SelectionSource::Unfilled,
                }),
                None => {
                    return Err(CompositionError::UnresolvedAxis {
                        axis: axis.name.clone(),
                        options: format!(
                            "; its options are {}",
                            crate::error::format_quoted_list(axis.options.keys())
                        ),
                    })
                }
            },
        }
    }

    Ok(ComponentSelection { entries })
}

/// One line per axis naming its options, for the "names no option of this
/// launcher" refusals: the fix is picking from the list, so the list is the
/// error.
fn axes_menu(axes: &[ComponentAxis]) -> String {
    axes.iter()
        .map(|axis| {
            format!(
                "\n  - {}: {}",
                axis.name,
                crate::error::format_quoted_list(axis.options.keys())
            )
        })
        .collect()
}

/// Every legal selection of these axes, in a fixed order: axis by axis in
/// declaration order, option by option in declaration order, an unfilled
/// entry last on optional axes. Used by `repo index --check` to hold the
/// whole selection space to the same structural checks a launch would run.
pub fn enumerate_selections(axes: &[ComponentAxis]) -> Vec<ComponentSelection> {
    let mut selections = vec![ComponentSelection::default()];
    for axis in axes {
        let mut next = Vec::with_capacity(selections.len() * (axis.options.len() + 1));
        for selection in &selections {
            for option in axis.options.keys() {
                let mut extended = selection.clone();
                extended.entries.push(SelectionEntry {
                    axis: axis.name.clone(),
                    option: Some(option.clone()),
                    source: SelectionSource::Explicit,
                });
                next.push(extended);
            }
            if axis.optional {
                let mut extended = selection.clone();
                extended.entries.push(SelectionEntry {
                    axis: axis.name.clone(),
                    option: None,
                    source: SelectionSource::Unfilled,
                });
                next.push(extended);
            }
        }
        selections = next;
    }
    selections
}

/// The size of the selection space, saturating rather than multiplying out:
/// callers use it to decide whether enumerating is reasonable, which it must
/// answer without doing the enumeration.
pub fn selection_space_size(axes: &[ComponentAxis]) -> usize {
    axes.iter()
        .map(|axis| axis.options.len() + usize::from(axis.optional))
        .fold(1usize, |acc, per_axis| acc.saturating_mul(per_axis))
}

/// One fragment of one option, loaded and paired with the label the resolve
/// report and the error messages name it by: the path as written for a file,
/// `inline option `axis.option`` for a body written in the launcher.
pub struct LoadedFragment {
    pub fragment: super::composition::Fragment,
    pub origin: String,
}

/// Every option's fragments, loaded: file fragments read and validated,
/// inline fragments taken from the launcher document. Loading ALL options,
/// not just the selected ones, is what lets a launch refuse the same things
/// `repo index --check` refuses (an option that does not provide its axis's
/// interface, a guard naming an unknown axis) instead of discovering them
/// one selection at a time.
pub struct LoadedComposition {
    pub options: BTreeMap<String, BTreeMap<String, Vec<LoadedFragment>>>,
}

/// Reads every file fragment the launcher references and runs the
/// selection-independent checks: path safety, fragment parse, `provides`
/// satisfaction, guard references, and that every adjustment target is
/// defined by some option of the launcher.
pub fn load_composition(
    launcher: &PeppyLauncher,
    launcher_dir: &Path,
) -> Result<LoadedComposition, CompositionError> {
    let axes = &launcher.components;
    let mut options: BTreeMap<String, BTreeMap<String, Vec<LoadedFragment>>> = BTreeMap::new();
    let launcher_label = launcher_file_label(launcher_dir);

    for axis in axes {
        let mut loaded_axis = BTreeMap::new();
        for (option_name, spec) in &axis.options {
            let mut loaded = Vec::with_capacity(spec.0.len());
            for part in &spec.0 {
                match part {
                    FragmentPart::Inline(fragment) => loaded.push(LoadedFragment {
                        fragment: fragment.clone(),
                        origin: format!("inline option `{}.{}`", axis.name, option_name),
                    }),
                    FragmentPart::File(raw) => {
                        let origin = format!("{launcher_label}, option `{}.{}`", axis.name, option_name);
                        loaded.push(load_fragment_file(launcher_dir, raw, &origin)?);
                    }
                }
            }
            loaded_axis.insert(option_name.clone(), loaded);
        }
        options.insert(axis.name.clone(), loaded_axis);
    }

    // Every option defines its axis's interface, whatever else it privately
    // defines: that is the promise a link or adjustment against a `provides`
    // id rests on.
    for axis in axes {
        for (option_name, loaded) in &options[axis.name.as_str()] {
            let defined: HashSet<&str> = loaded
                .iter()
                .flat_map(|fragment| fragment.fragment.deployments.iter())
                .flat_map(|deployment| deployment.instances.iter())
                .map(|instance| instance.instance_id.as_str())
                .collect();
            for id in &axis.provides {
                if !defined.contains(id.as_str()) {
                    return Err(CompositionError::ProvidesUnmet {
                        axis: axis.name.clone(),
                        option: option_name.clone(),
                        id: id.to_string(),
                    });
                }
            }
        }
    }

    // Guards of file fragments reference the launcher's axes; the inline ones
    // were checked when the launcher parsed.
    for axis in axes {
        for loaded in options[axis.name.as_str()].values() {
            for fragment in loaded {
                for adjustment in &fragment.fragment.adjustments {
                    if let Some(when) = &adjustment.when {
                        validate_guard(when, axes, &fragment.origin)
                            .map_err(|reason| CompositionError::FragmentInvalid {
                                path: fragment.origin.clone(),
                                origin: launcher_label.clone(),
                                detail: reason,
                            })?;
                    }
                }
            }
        }
    }

    // An adjustment target the selection does not define is skipped; a
    // target NOTHING can define is a dead reference, and refusing it here
    // catches it once for every selection.
    let mut definable: HashSet<&str> = launcher
        .deployments
        .iter()
        .flat_map(|d| d.instances.iter())
        .map(|i| i.instance_id.as_str())
        .collect();
    for fragment in options
        .values()
        .flat_map(|axis| axis.values())
        .flat_map(|fragments| fragments.iter())
    {
        for deployment in &fragment.fragment.deployments {
            for instance in &deployment.instances {
                definable.insert(instance.instance_id.as_str());
            }
        }
    }
    for adjustment in &launcher.adjustments {
        if !definable.contains(adjustment.target.as_str()) {
            return Err(CompositionError::TargetDefinedNowhere {
                target: adjustment.target.to_string(),
                origin: format!("{launcher_label} (base)"),
            });
        }
    }
    for fragment in options
        .values()
        .flat_map(|axis| axis.values())
        .flat_map(|fragments| fragments.iter())
    {
        for adjustment in &fragment.fragment.adjustments {
            if !definable.contains(adjustment.target.as_str()) {
                return Err(CompositionError::TargetDefinedNowhere {
                    target: adjustment.target.to_string(),
                    origin: fragment.origin.clone(),
                });
            }
        }
    }

    Ok(LoadedComposition { options })
}

/// The label error messages and reports name a launcher by: its file name,
/// or the directory's name when the launcher path itself was elided.
fn launcher_file_label(launcher_dir: &Path) -> String {
    launcher_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| launcher_dir.display().to_string())
}

fn load_fragment_file(
    launcher_dir: &Path,
    raw: &str,
    origin: &str,
) -> Result<LoadedFragment, CompositionError> {
    let path = resolve_fragment_path(launcher_dir, raw, origin)?;
    let content = std::fs::read_to_string(&path).map_err(|e| CompositionError::FragmentUnreadable {
        path: raw.to_owned(),
        origin: origin.to_owned(),
        detail: e.to_string(),
    })?;
    if content.trim().is_empty() {
        return Err(CompositionError::FragmentUnreadable {
            path: raw.to_owned(),
            origin: origin.to_owned(),
            detail: "the file is empty".to_owned(),
        });
    }
    let parsed = LauncherFragmentParser::from_content(&content).map_err(|e| {
        CompositionError::FragmentInvalid {
            path: raw.to_owned(),
            origin: origin.to_owned(),
            detail: e.to_string(),
        }
    })?;
    for adjustment in &parsed.body.adjustments {
        validate_adjustment(adjustment, raw).map_err(|reason| CompositionError::FragmentInvalid {
            path: raw.to_owned(),
            origin: origin.to_owned(),
            detail: reason,
        })?;
    }
    Ok(LoadedFragment {
        fragment: parsed.body,
        origin: raw.to_owned(),
    })
}

/// A fragment path must be relative, plain (no `.` or `..` segments), and
/// must stay inside the launcher's directory even through symlinks: a
/// repository launcher and a filesystem launcher resolve fragments
/// identically, and neither reaches outside the tree it lives in.
fn resolve_fragment_path(
    launcher_dir: &Path,
    raw: &str,
    origin: &str,
) -> Result<PathBuf, CompositionError> {
    let refuse = |reason: &str| {
        Err(CompositionError::FragmentPath {
            path: raw.to_owned(),
            origin: origin.to_owned(),
            reason: reason.to_owned(),
        })
    };

    let relative = Path::new(raw);
    if relative.is_absolute() {
        return refuse("it is absolute");
    }
    let mut cleaned = PathBuf::new();
    for component in relative.components() {
        match component {
            PathComponent::Normal(segment) => cleaned.push(segment),
            PathComponent::CurDir => return refuse("it contains a `.` segment"),
            PathComponent::ParentDir => return refuse("it contains a `..` segment"),
            _ => return refuse("it is not a plain relative path"),
        }
    }
    if cleaned.as_os_str().is_empty() {
        return refuse("it is empty");
    }

    let base = launcher_dir.canonicalize().map_err(|e| {
        CompositionError::FragmentUnreadable {
            path: raw.to_owned(),
            origin: origin.to_owned(),
            detail: format!("the launcher's directory cannot be resolved: {e}"),
        }
    })?;
    let full = launcher_dir.join(&cleaned);
    let canonical = full.canonicalize().map_err(|e| {
        CompositionError::FragmentUnreadable {
            path: raw.to_owned(),
            origin: origin.to_owned(),
            detail: e.to_string(),
        }
    })?;
    if !canonical.starts_with(&base) {
        return refuse("it escapes the launcher's directory (through a symlink)");
    }
    Ok(full)
}

/// What one applied adjustment did, for the resolve report: the target and
/// field it touched, the before and after, and the fragment (or base) it
/// came from.
#[derive(Debug, Clone)]
pub struct AppliedAdjustment {
    pub target: String,
    /// `arguments.<key>` or `links.<slot>`.
    pub field: String,
    pub change: AppliedChange,
    pub origin: String,
}

#[derive(Debug, Clone)]
pub enum AppliedChange {
    Set {
        old: Option<String>,
        new: String,
    },
    Added {
        target: String,
    },
    Removed {
        old: String,
    },
}

impl AppliedChange {
    fn render(&self) -> String {
        match self {
            AppliedChange::Set {
                old: Some(old),
                new,
            } => format!("{old} -> {new}"),
            AppliedChange::Set { old: None, new } => format!("(absent) -> {new}"),
            AppliedChange::Added { target } => format!("+ {target}"),
            AppliedChange::Removed { old } => format!("removed (was {old})"),
        }
    }
}

/// Why an adjustment did not run.
#[derive(Debug, Clone)]
pub enum SkipReason {
    /// The target the selection resolved to does not define this instance:
    /// the recorder's attach simply does not run when no recorder was
    /// selected.
    TargetAbsent,
    /// The `when` guard named an axis whose selected option is not the one
    /// the guard requires.
    GuardNotMet(String),
}

impl SkipReason {
    fn render(&self) -> String {
        match self {
            SkipReason::TargetAbsent => "target is not in this selection".to_owned(),
            SkipReason::GuardNotMet(guard) => format!("guard {guard} is not met"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SkippedAdjustment {
    pub target: String,
    pub reason: SkipReason,
    pub origin: String,
}

/// The audit trail of one flattening: what was selected, which adjustments
/// applied (with the values they replaced), and which were skipped (and
/// why). `peppy stack resolve` prints it; a launch holds it for the echo.
#[derive(Debug, Clone, Default)]
pub struct FlattenReport {
    pub selection: ComponentSelection,
    pub applied: Vec<AppliedAdjustment>,
    pub skipped: Vec<SkippedAdjustment>,
}

impl FlattenReport {
    /// The report body `peppy stack resolve` writes to stderr, one entry per
    /// line, applied before skipped.
    pub fn render_lines(&self) -> Vec<String> {
        let mut lines = vec![format!("components: {}", self.selection.echo())];
        if !self.applied.is_empty() {
            lines.push(String::from("adjustments applied:"));
            for entry in &self.applied {
                lines.push(format!(
                    "  {}.{}: {}  ({})",
                    entry.target, entry.field, entry.change.render(), entry.origin
                ));
            }
        }
        if !self.skipped.is_empty() {
            lines.push(String::from("adjustments skipped:"));
            for entry in &self.skipped {
                lines.push(format!(
                    "  {}: {}  ({})",
                    entry.target,
                    entry.reason.render(),
                    entry.origin
                ));
            }
        }
        lines
    }
}

/// One adjustment paired with where it came from and whether it speaks for
/// a fragment (bound by the no-fighting rules) or for the base (which may
/// override anything a fragment set).
struct PlannedAdjustment<'a> {
    adjustment: &'a Adjustment,
    origin: String,
    /// Identifies the fragment an adjustment belongs to for the conflict
    /// rules: two adjustments from one fragment are one author applying list
    /// order, two from different fragments are two authors fighting.
    fragment_id: usize,
    is_base: bool,
}

/// Flattens a composed launcher with a resolved selection into the ordinary
/// flat launcher the existing pipeline consumes, exactly as if the author
/// had written it by hand.
///
/// The flattened document goes back through the flat launcher's own
/// validation (serialize, re-parse), so every whole-document check a
/// hand-written file gets runs on the composed result too.
pub fn flatten(
    launcher: &PeppyLauncher,
    loaded: &LoadedComposition,
    selection: &ComponentSelection,
    base_label: &str,
) -> Result<(PeppyLauncher, FlattenReport), CompositionError> {
    // 1-2. Collect: base deployments first, then each selected option's
    // fragments in axis declaration order and fragment list order.
    let mut collected: Vec<(&Deployment, String)> = launcher
        .deployments
        .iter()
        .map(|deployment| (deployment, base_label.to_owned()))
        .collect();
    let mut selected_fragments: Vec<&LoadedFragment> = Vec::new();
    for entry in &selection.entries {
        let Some(option) = &entry.option else { continue };
        let Some(fragments) = loaded
            .options
            .get(entry.axis.as_str())
            .and_then(|axis| axis.get(option.as_str()))
        else {
            continue;
        };
        selected_fragments.extend(fragments.iter());
    }
    for fragment in &selected_fragments {
        for deployment in &fragment.fragment.deployments {
            collected.push((deployment, fragment.origin.clone()));
        }
    }

    // 3. Instance-id uniqueness across the collected set, naming both
    // origins. Distinct options of one axis may share ids by design, but
    // only one option per axis is ever selected.
    let mut first_origin: HashMap<&str, &str> = HashMap::new();
    for (deployment, origin) in &collected {
        for instance in &deployment.instances {
            let id = instance.instance_id.as_str();
            if let Some(first) = first_origin.insert(id, origin) {
                return Err(CompositionError::DuplicateInstanceId {
                    id: id.to_owned(),
                    first: first.to_owned(),
                    second: origin.clone(),
                });
            }
        }
    }

    // 2 (again, structurally): merge entries sharing a source into one
    // entry whose instance list is the union, in collection order. The
    // daemon resolves one deployment per name:tag, so this merge is what
    // lets a base and a fragment both deploy `uvc_camera_linux`.
    let mut merged: Vec<Deployment> = Vec::with_capacity(collected.len());
    for &(deployment, _) in &collected {
        match merged
            .iter_mut()
            .find(|entry| entry.source == deployment.source)
        {
            Some(entry) => entry.instances.extend(deployment.instances.iter().cloned()),
            None => merged.push(deployment.clone()),
        }
    }

    // 4. Union core node links: base first, then fragments in collection
    // order; a link declared by both is one link.
    let mut core_nodes: Vec<String> = launcher.core_nodes.clone();
    for fragment in &selected_fragments {
        for link in &fragment.fragment.core_nodes {
            if !core_nodes.contains(link) {
                core_nodes.push(link.clone());
            }
        }
    }

    // 5. Plan the adjustments: fragments in collection order, then the base
    // in list order. A plan step runs only if its guard holds and its
    // target is in this selection; each skip is recorded.
    let base_origin = format!("{base_label} (base)");
    let mut planned: Vec<PlannedAdjustment> = Vec::new();
    let mut skipped: Vec<SkippedAdjustment> = Vec::new();
    for (fragment_id, fragment) in selected_fragments.iter().enumerate() {
        for adjustment in &fragment.fragment.adjustments {
            planned.push(PlannedAdjustment {
                adjustment,
                origin: fragment.origin.clone(),
                fragment_id,
                is_base: false,
            });
        }
    }
    for adjustment in &launcher.adjustments {
        planned.push(PlannedAdjustment {
            adjustment,
            origin: base_origin.clone(),
            fragment_id: usize::MAX,
            is_base: true,
        });
    }
    let mut running: Vec<&PlannedAdjustment> = Vec::new();
    for step in &planned {
        if let Some(when) = &step.adjustment.when
            && !guard_holds(selection, when)
        {
            skipped.push(SkippedAdjustment {
                target: step.adjustment.target.to_string(),
                reason: SkipReason::GuardNotMet(render_guard(when)),
                origin: step.origin.clone(),
            });
            continue;
        }
        if !first_origin.contains_key(step.adjustment.target.as_str()) {
            skipped.push(SkippedAdjustment {
                target: step.adjustment.target.to_string(),
                reason: SkipReason::TargetAbsent,
                origin: step.origin.clone(),
            });
            continue;
        }
        running.push(step);
    }

    // 6. Conflict rules among surviving FRAGMENT adjustments. The base is
    // not a second author fighting a fragment; it is the one author who
    // owns the file, and its later entries win over its earlier ones.
    let mut writers: HashMap<(&str, KeySpace, &str), (usize, String)> = HashMap::new();
    let mut adders: HashMap<(&str, &str), (usize, String)> = HashMap::new();
    for step in &running {
        if step.is_base {
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
        for key in step.adjustment.add_links.iter().flat_map(|links| links.keys()) {
            let entry = adders
                .entry((target, key.as_str()))
                .or_insert((step.fragment_id, step.origin.clone()));
            entry.1 = step.origin.clone();
        }
    }
    for ((target, key), (adder_id, adder_origin)) in &adders {
        if let Some((writer_id, writer_origin)) =
            writers.get(&(target, KeySpace::Links, key))
            && *writer_id != *adder_id
        {
            return Err(CompositionError::AdjustmentsConflict {
                target: (*target).to_owned(),
                field: format!("links.{key}"),
                first: writer_origin.clone(),
                second: adder_origin.clone(),
            });
        }
    }

    // 7. Apply. Instances are located by id across the merged deployments.
    // The map is keyed by owned ids: the apply loop mutates `merged` while
    // looking targets up in it.
    let mut index: HashMap<String, (usize, usize)> = HashMap::new();
    for (deployment_idx, deployment) in merged.iter().enumerate() {
        for (instance_idx, instance) in deployment.instances.iter().enumerate() {
            index.insert(instance.instance_id.to_string(), (deployment_idx, instance_idx));
        }
    }
    let mut applied: Vec<AppliedAdjustment> = Vec::new();
    for step in &running {
        let &(deployment_idx, instance_idx) = index
            .get(step.adjustment.target.as_str())
            .expect("target presence checked when the plan was built");
        let instance = &mut merged[deployment_idx].instances[instance_idx];
        let origin = step.origin.clone();
        let target = step.adjustment.target.to_string();
        if let Some(arguments) = &step.adjustment.set_arguments {
            for (key, value) in arguments {
                applied.push(AppliedAdjustment {
                    field: format!("arguments.{key}"),
                    change: AppliedChange::Set {
                        old: instance
                            .arguments
                            .get(key)
                            .map(render_value)
                            .or(Some(String::from("(absent)"))),
                        new: render_value(value),
                    },
                    target: target.clone(),
                    origin: origin.clone(),
                });
                instance.arguments.insert(key.clone(), value.clone());
            }
        }
        if let Some(links) = &step.adjustment.set_links {
            for (slot, value) in links {
                applied.push(AppliedAdjustment {
                    field: format!("links.{slot}"),
                    change: AppliedChange::Set {
                        old: instance.links.get(slot).map(render_link).or(Some(String::from("(absent)"))),
                        new: render_link(value),
                    },
                    target: target.clone(),
                    origin: origin.clone(),
                });
                instance.links.insert(slot.clone(), value.clone());
            }
        }
        if let Some(links) = &step.adjustment.add_links {
            for (slot, additions) in links {
                match instance.links.get(slot) {
                    None | Some(LinkValue::Bound(Selection::Array(_))) => {
                        let mut targets = instance
                            .links
                            .get(slot)
                            .and_then(|value| value.selection().cloned())
                            .map(|selection| match selection {
                                Selection::Array(targets) => targets.as_slice().to_vec(),
                                _ => unreachable!("matched an array binding above"),
                            })
                            .unwrap_or_default();
                        for addition in additions {
                            if targets.contains(addition) {
                                return Err(CompositionError::AddLinksDuplicateTarget {
                                    origin: origin.clone(),
                                    target: target.clone(),
                                    slot: slot.clone(),
                                    added: addition.clone(),
                                });
                            }
                            applied.push(AppliedAdjustment {
                                field: format!("links.{slot}"),
                                change: AppliedChange::Added {
                                    target: addition.clone(),
                                },
                                target: target.clone(),
                                origin: origin.clone(),
                            });
                            targets.push(addition.clone());
                        }
                        let bound = LinkTargets::new(targets).map_err(|err| {
                            CompositionError::AddLinksDuplicateTarget {
                                origin: origin.clone(),
                                target: target.clone(),
                                slot: slot.clone(),
                                added: err.target,
                            }
                        })?;
                        instance
                            .links
                            .insert(slot.clone(), LinkValue::Bound(Selection::Array(bound)));
                    }
                    Some(LinkValue::Bound(Selection::Scalar(_))) => {
                        return Err(CompositionError::AddLinksOnNonArray {
                            origin: origin.clone(),
                            target: target.clone(),
                            slot: slot.clone(),
                            holds: "a scalar binding",
                        })
                    }
                    Some(LinkValue::Bound(Selection::Flags(_))) => {
                        return Err(CompositionError::AddLinksOnNonArray {
                            origin: origin.clone(),
                            target: target.clone(),
                            slot: slot.clone(),
                            holds: "a flag-accumulated binding",
                        })
                    }
                    Some(LinkValue::Vacant(_)) => {
                        return Err(CompositionError::AddLinksOnNonArray {
                            origin: origin.clone(),
                            target: target.clone(),
                            slot: slot.clone(),
                            holds: "a vacancy",
                        })
                    }
                }
            }
        }
        if let Some(slots) = &step.adjustment.unset_links {
            for slot in slots {
                if let Some(old) = instance.links.remove(slot) {
                    applied.push(AppliedAdjustment {
                        field: format!("links.{slot}"),
                        change: AppliedChange::Removed {
                            old: render_link(&old),
                        },
                        target: target.clone(),
                        origin: origin.clone(),
                    });
                }
            }
        }
    }

    // 8. The flat document is validated by the same parse a hand-written
    // launcher goes through, so link-target resolution, sentinel keys, and
    // the core-node checks all run on the composed result.
    let flat = PeppyLauncher {
        peppy_schema: launcher.peppy_schema,
        core_nodes,
        deployments: merged,
        components: Vec::new(),
        adjustments: Vec::new(),
    };
    let text = serde_json5::to_string(&flat).map_err(|e| CompositionError::FlatValidation(
        crate::error::Error::Serialize(e.to_string()),
    ))?;
    let validated = PeppyLauncherParser::from_content(&text)?;

    Ok((
        validated,
        FlattenReport {
            selection: selection.clone(),
            applied,
            skipped,
        },
    ))
}

/// The two key spaces an adjustment can write in. `add_links` shares the
/// links space with `set_links` and `unset_links`: appending to a slot and
/// replacing it are two claims on the same entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// Records one fragment's claim on a `(target, key)` write slot, refusing
/// when a DIFFERENT fragment already claimed it. Two adjustments from one
/// fragment are one author applying list order (the later wins, the
/// earlier's claim is simply superseded); two from different fragments are
/// two authors, and no last-writer-wins between them is allowed to be
/// silent.
fn claim_write<'a>(
    writers: &mut HashMap<(&'a str, KeySpace, &'a str), (usize, String)>,
    target: &'a str,
    space: KeySpace,
    key: &'a str,
    step: &PlannedAdjustment<'_>,
) -> Result<(), CompositionError> {
    let entry = writers
        .entry((target, space, key))
        .or_insert((step.fragment_id, step.origin.clone()));
    if entry.0 != step.fragment_id {
        return Err(CompositionError::AdjustmentsConflict {
            target: target.to_owned(),
            field: format!("{}.{key}", space.label()),
            first: entry.1.clone(),
            second: step.origin.clone(),
        });
    }
    entry.1 = step.origin.clone();
    Ok(())
}

/// Every guard entry must match the selection: naming several axes is an
/// AND, which is how a base writes "only when the headset leads the real
/// robot".
fn guard_holds(selection: &ComponentSelection, when: &BTreeMap<String, String>) -> bool {
    when.iter()
        .all(|(axis, option)| selection.option_on(axis) == Some(option.as_str()))
}

fn render_guard(when: &BTreeMap<String, String>) -> String {
    when.iter()
        .map(|(axis, option)| format!("{axis}={option}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_value(value: &config::AnyType) -> String {
    serde_json5::to_string(value).unwrap_or_else(|_| String::from("(unrenderable)"))
}

fn render_link(value: &LinkValue) -> String {
    serde_json5::to_string(value).unwrap_or_else(|_| String::from("(unrenderable)"))
}

/// Resolve the `--with` words against a launcher and flatten the result.
///
/// The one entry point the launch paths and `peppy stack resolve` share: a
/// flat launcher with no words passes through unchanged, a flat launcher
/// with words is refused, and a composed launcher is loaded, resolved, and
/// flattened into an ordinary `launcher/v1` document.
pub fn compose(
    launcher: &PeppyLauncher,
    launcher_file: &Path,
    words: &[String],
) -> Result<(PeppyLauncher, FlattenReport), CompositionError> {
    if launcher.components.is_empty() {
        if words.is_empty() {
            return Ok((launcher.clone(), FlattenReport::default()));
        }
        return Err(CompositionError::WithOnFlatLauncher);
    }

    let launcher_dir = match launcher_file.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let loaded = load_composition(launcher, &launcher_dir)?;
    let selection = resolve_selection(&launcher.components, words)?;
    let base_label = launcher_file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| launcher_file.display().to_string());
    flatten(launcher, &loaded, &selection, &base_label)
}

#[cfg(test)]
mod tests {
    use super::super::composition::FragmentSpec;
    use super::*;
    use crate::launcher::{ComponentAxis, PeppyLauncherParser};
    use std::fs;
    use tempfile::tempdir;

    fn write(path: &Path, content: &str) -> PathBuf {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
        path.to_path_buf()
    }

    fn parse_launcher(content: &str) -> PeppyLauncher {
        PeppyLauncherParser::from_content(content).expect("launcher parses")
    }

    fn fragment_file(body: &str) -> String {
        format!("{{ peppy_schema: \"launcher_fragment/v1\", {body} }}")
    }

    /// The worked micro-example: one base, a robot axis with an inline real
    /// option and a file-backed sim option sharing a relay fragment, an
    /// optional recorder axis whose attach adjustment reaches whichever
    /// commander was selected.
    fn composed_fixture(dir: &Path) -> (PathBuf, PeppyLauncher) {
        write(
            &dir.join("fragments/sim_relays.json5"),
            &fragment_file(r#"
                deployments: [
                    { source: { name: "arm_sim", tag: "v1" },
                      instances: [{ instance_id: "arm_inst", links: { engine: "sim_inst/arm" } }] },
                ],
                adjustments: [
                    { target: "backbone_inst", set_arguments: { max_ee_velocity_m_s: 0.5 } },
                ],
            "#),
        );
        write(
            &dir.join("fragments/mujoco_engine.json5"),
            &fragment_file(r#"
                deployments: [
                    { source: { name: "sim_mujoco", tag: "v1" },
                      instances: [{ instance_id: "sim_inst", arguments: { state_rate_hz: 100 } }] },
                ],
                adjustments: [
                    { target: "recorder_inst",
                      set_arguments: { robot_type: "openarm_mujoco" } },
                ],
            "#),
        );
        write(
            &dir.join("fragments/recorder.json5"),
            &fragment_file(r#"
                deployments: [
                    { source: { name: "recorder", tag: "v1" },
                      instances: [{ instance_id: "recorder_inst",
                                    links: { observed: ["arm_inst"] } }] },
                ],
                adjustments: [
                    { target: "commander_inst", add_links: { recorder: ["recorder_inst"] } },
                ],
            "#),
        );
        let launcher = write(
            &dir.join("teleop.json5"),
            r#"{
                peppy_schema: "launcher/v1",
                components: [
                    { name: "robot",
                      provides: ["arm_inst"],
                      default: "real",
                      options: {
                          real: { deployments: [
                              { source: { name: "can_arm", tag: "v1" },
                                instances: [{ instance_id: "arm_inst" }] } ] },
                          mujoco: ["fragments/sim_relays.json5", "fragments/mujoco_engine.json5"],
                      } },
                    { name: "recorder",
                      optional: true,
                      provides: ["recorder_inst"],
                      options: { on: "fragments/recorder.json5" } },
                ],
                adjustments: [
                    { target: "sim_inst", set_arguments: { hardware_version: "v2" } },
                ],
                deployments: [
                    { source: { name: "backbone", tag: "v1" },
                      instances: [{ instance_id: "backbone_inst",
                                    links: { arm: "arm_inst" } }] },
                    { source: { name: "commander", tag: "v1" },
                      instances: [{ instance_id: "commander_inst",
                                    links: { backbone: "backbone_inst" } }] },
                ],
            }"#,
        );
        let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
        (launcher, parsed)
    }

    fn words(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|entry| entry.to_string()).collect()
    }

    #[test]
    fn defaults_produce_the_flat_document() {
        let dir = tempdir().unwrap();
        let (file, launcher) = composed_fixture(dir.path());
        let (flat, report) = compose(&launcher, &file, &[]).expect("composes");

        assert!(flat.components.is_empty());
        assert!(flat.adjustments.is_empty());
        // Base deployments first, then the real option's deployment.
        let sources: Vec<String> = flat
            .deployments
            .iter()
            .map(|d| format!("{}:{}", d.source.name, d.source.tag))
            .collect();
        assert_eq!(sources, ["backbone:v1", "commander:v1", "can_arm:v1"]);
        assert_eq!(report.selection.echo(), "robot=real (default)  recorder=(off)");
        // The base's sim specialization sleeps when the robot is real.
        assert!(report
            .skipped
            .iter()
            .any(|s| s.target == "sim_inst" && matches!(s.reason, SkipReason::TargetAbsent)));
    }

    #[test]
    fn a_selection_pulls_in_fragments_in_order() {
        let dir = tempdir().unwrap();
        let (file, launcher) = composed_fixture(dir.path());
        let (flat, report) = compose(&launcher, &file, &words(&["mujoco"])).expect("composes");

        // Base deployments first, then sim_relays, then the engine.
        let sources: Vec<String> = flat
            .deployments
            .iter()
            .map(|d| format!("{}:{}", d.source.name, d.source.tag))
            .collect();
        assert_eq!(
            sources,
            ["backbone:v1", "commander:v1", "arm_sim:v1", "sim_mujoco:v1"]
        );
        let _ = report;
        // The base adjustment reached the engine instance.
        let sim = flat
            .deployments
            .iter()
            .find(|d| d.source.name == "sim_mujoco")
            .unwrap();
        assert_eq!(
            sim.instances[0].arguments.get("hardware_version"),
            Some(&config::AnyType::String("v2".to_owned()))
        );
        // And the relay fragment's adjustment reached the base's backbone.
        let backbone = flat
            .deployments
            .iter()
            .find(|d| d.source.name == "backbone")
            .unwrap();
        assert_eq!(
            backbone.instances[0].arguments.get("max_ee_velocity_m_s"),
            Some(&config::AnyType::Float(0.5))
        );
        assert_eq!(report.selection.echo(), "robot=mujoco  recorder=(off)");
    }

    #[test]
    fn fragments_merge_into_a_base_deployment_of_the_same_source() {
        let dir = tempdir().unwrap();
        write(&dir.path().join("fragments/cam.json5"), &fragment_file(r#"
            deployments: [
                { source: { name: "camera", tag: "v1" },
                  instances: [{ instance_id: "wrist" }] },
            ],
        "#));
        let launcher = write(&dir.path().join("l.json5"), r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "cams", optional: true, provides: ["wrist"],
                  options: { on: "fragments/cam.json5" } },
            ],
            deployments: [
                { source: { name: "camera", tag: "v1" },
                  instances: [{ instance_id: "chest" }] },
            ],
        }"#);
        let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
        let (flat, _) = compose(&parsed, &launcher, &["on".to_string()]).expect("composes");
        assert_eq!(flat.deployments.len(), 1);
        assert_eq!(flat.deployments[0].source.name, "camera");
        let ids: Vec<&str> = flat.deployments[0]
            .instances
            .iter()
            .map(|i| i.instance_id.as_str())
            .collect();
        assert_eq!(ids, ["chest", "wrist"]);
    }

    #[test]
    fn a_duplicate_instance_id_names_both_origins() {
        let dir = tempdir().unwrap();
        write(&dir.path().join("fragments/dup.json5"), &fragment_file(r#"
            deployments: [
                { source: { name: "other", tag: "v1" },
                  instances: [{ instance_id: "arm_inst" }] },
            ],
        "#));
        let launcher = write(&dir.path().join("l.json5"), r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "robot", optional: true, provides: ["arm_inst"],
                  options: { sim: "fragments/dup.json5" } },
            ],
            deployments: [
                { source: { name: "can_arm", tag: "v1" },
                  instances: [{ instance_id: "arm_inst" }] },
            ],
        }"#);
        let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
        let err = compose(&parsed, &launcher, &["sim".to_string()]).expect_err("duplicate id");
        assert!(err.to_string().contains("arm_inst"), "got: {err}");
        assert!(err.to_string().contains("l.json5"), "got: {err}");
        assert!(err.to_string().contains("fragments/dup.json5"), "got: {err}");
    }

    #[test]
    fn add_links_appends_across_fragments_in_order() {
        let dir = tempdir().unwrap();
        write(&dir.path().join("fragments/a.json5"), &fragment_file(r#"
            deployments: [],
            adjustments: [
                { target: "panel", add_links: { observers: ["a_inst"] } },
            ],
        "#));
        write(&dir.path().join("fragments/b.json5"), &fragment_file(r#"
            deployments: [],
            adjustments: [
                { target: "panel", add_links: { observers: ["b_inst"] } },
            ],
        "#));
        let launcher = write(&dir.path().join("l.json5"), r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "a", optional: true, options: { on: "fragments/a.json5" } },
                { name: "b", optional: true, options: { on: "fragments/b.json5" } },
            ],
            deployments: [
                { source: { name: "panel", tag: "v1" },
                  instances: [{ instance_id: "panel",
                                links: { observers: ["seed_inst"] } }] },
                { source: { name: "seed", tag: "v1" }, instances: [{ instance_id: "seed_inst" }] },
                { source: { name: "a", tag: "v1" }, instances: [{ instance_id: "a_inst" }] },
                { source: { name: "b", tag: "v1" }, instances: [{ instance_id: "b_inst" }] },
            ],
        }"#);
        let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
        let (flat, report) = compose(&parsed, &launcher, &["a=on".to_string(), "b=on".to_string()])
            .expect("composes");
        let panel = &flat.deployments[0].instances[0];
        let LinkValue::Bound(Selection::Array(targets)) = &panel.links["observers"] else {
            panic!("observers stays an array binding");
        };
        assert_eq!(targets.as_slice(), ["seed_inst", "a_inst", "b_inst"]);
        let added: Vec<&str> = report
            .applied
            .iter()
            .filter(|a| a.field == "links.observers")
            .map(|a| match &a.change {
                AppliedChange::Added { target } => target.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(added, ["a_inst", "b_inst"]);
    }

    #[test]
    fn add_links_refuses_a_scalar_and_a_vacancy() {
        for (slot_value, holds) in [
            ("\"x_inst\"", "a scalar binding"),
            ("{ vacant: \"nothing here\" }", "a vacancy"),
        ] {
            let dir = tempdir().unwrap();
            write(
                &dir.path().join("fragments/a.json5"),
                &fragment_file(
                    r#"adjustments: [ { target: "panel", add_links: { observers: ["a_inst"] } } ],"#,
                ),
            );
            let launcher = write(
                &dir.path().join("l.json5"),
                &format!(
                    r#"{{
                        peppy_schema: "launcher/v1",
                        components: [
                            {{ name: "a", optional: true, options: {{ on: "fragments/a.json5" }} }},
                        ],
                        deployments: [
                            {{ source: {{ name: "panel", tag: "v1" }},
                              instances: [{{ instance_id: "panel",
                                            links: {{ observers: {slot_value} }} }}] }},
                            {{ source: {{ name: "a", tag: "v1" }}, instances: [{{ instance_id: "a_inst" }}] }},
                        ],
                    }}"#
                ),
            );
            let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
            let err = compose(&parsed, &launcher, &words(&["on"]))
                .expect_err("appending past a non-array must be refused");
            let CompositionError::AddLinksOnNonArray { holds: actual, .. } = &err else {
                panic!("expected AddLinksOnNonArray, got: {err}");
            };
            assert_eq!(*actual, holds);
        }
    }

    #[test]
    fn add_links_refuses_a_target_already_bound() {
        let dir = tempdir().unwrap();
        write(&dir.path().join("fragments/a.json5"), &fragment_file(r#"
            adjustments: [ { target: "panel", add_links: { observers: ["seed_inst"] } } ],
        "#));
        let launcher = write(&dir.path().join("l.json5"), r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "a", optional: true, options: { on: "fragments/a.json5" } },
            ],
            deployments: [
                { source: { name: "panel", tag: "v1" },
                  instances: [{ instance_id: "panel",
                                links: { observers: ["seed_inst"] } }] },
            ],
        }"#);
        let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
        let err = compose(&parsed, &launcher, &["on".to_string()]).expect_err("duplicate add");
        assert!(err.to_string().contains("seed_inst"), "got: {err}");
    }

    #[test]
    fn two_fragments_writing_one_key_are_refused() {
        let dir = tempdir().unwrap();
        write(&dir.path().join("fragments/a.json5"), &fragment_file(r#"
            adjustments: [ { target: "panel", set_arguments: { rate: 10 } } ],
        "#));
        write(&dir.path().join("fragments/b.json5"), &fragment_file(r#"
            adjustments: [ { target: "panel", set_arguments: { rate: 20 } } ],
        "#));
        let launcher = write(&dir.path().join("l.json5"), r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "a", optional: true, options: { on: "fragments/a.json5" } },
                { name: "b", optional: true, options: { on: "fragments/b.json5" } },
            ],
            deployments: [
                { source: { name: "panel", tag: "v1" }, instances: [{ instance_id: "panel" }] },
            ],
        }"#);
        let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
        let err = compose(&parsed, &launcher, &["a=on".to_string(), "b=on".to_string()])
            .expect_err("fragments must not fight");
        let CompositionError::AdjustmentsConflict { field, first, second, .. } = &err else {
            panic!("expected AdjustmentsConflict, got: {err}");
        };
        assert_eq!(field, "arguments.rate");
        assert!(first.contains("a.json5") && second.contains("b.json5"), "got: {first} / {second}");
    }

    #[test]
    fn the_base_overrides_a_fragment_and_a_later_base_entry_wins() {
        let dir = tempdir().unwrap();
        write(&dir.path().join("fragments/a.json5"), &fragment_file(r#"
            adjustments: [ { target: "panel", set_arguments: { rate: 10 } } ],
        "#));
        let launcher = write(&dir.path().join("l.json5"), r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "a", optional: true, options: { on: "fragments/a.json5" } },
            ],
            adjustments: [
                { target: "panel", set_arguments: { rate: 20 } },
                { target: "panel", set_arguments: { rate: 30 } },
            ],
            deployments: [
                { source: { name: "panel", tag: "v1" }, instances: [{ instance_id: "panel" }] },
            ],
        }"#);
        let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
        let (flat, report) = compose(&parsed, &launcher, &["on".to_string()]).expect("composes");
        assert_eq!(
            flat.deployments[0].instances[0].arguments.get("rate"),
            Some(&config::AnyType::Int(30))
        );
        // Both base entries and the fragment's contribution are visible.
        let rates: Vec<String> = report
            .applied
            .iter()
            .filter(|a| a.field == "arguments.rate")
            .map(|a| match &a.change {
                AppliedChange::Set { old, new } => format!("{}->{}", old.clone().unwrap_or_default(), new),
                _ => String::new(),
            })
            .collect();
        assert_eq!(rates, ["(absent)->10", "10->20", "20->30"]);
    }

    #[test]
    fn guards_skip_and_absent_targets_skip() {
        let dir = tempdir().unwrap();
        write(&dir.path().join("fragments/xr.json5"), &fragment_file(r#"
            deployments: [
                { source: { name: "xr", tag: "v1" },
                  instances: [{ instance_id: "leader_inst" }] },
            ],
            adjustments: [
                { target: "backbone_inst",
                  set_arguments: { upstream_mode: "pose" } },
            ],
        "#));
        write(&dir.path().join("fragments/rec.json5"), &fragment_file(r#"
            deployments: [
                { source: { name: "rec", tag: "v1" },
                  instances: [{ instance_id: "recorder_inst" }] },
            ],
            adjustments: [
                // A shape condition: only the XR leader pairs with the
                // backbone this way.
                { target: "backbone_inst", when: { commander: "xr" },
                  set_arguments: { record_backbone: true } },
                // Reaches into whichever robot was selected; skipped when the
                // robot is real and no sim_inst exists.
                { target: "sim_inst",
                  set_arguments: { state_rate_hz: 50 } },
            ],
        "#));
        let launcher = write(&dir.path().join("l.json5"), r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "commander", default: "web",
                  options: { web: { deployments: [] }, xr: "fragments/xr.json5" } },
                { name: "recorder", optional: true,
                  options: { on: "fragments/rec.json5" } },
                { name: "robot", default: "real",
                  options: {
                      real: { deployments: [] },
                      mujoco: { deployments: [
                          { source: { name: "sim_mujoco", tag: "v1" },
                            instances: [{ instance_id: "sim_inst" }] } ] },
                  } },
            ],
            deployments: [
                { source: { name: "backbone", tag: "v1" },
                  instances: [{ instance_id: "backbone_inst" }] },
            ],
        }"#);
        let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());

        // web + recorder: the guard is not met and sim_inst is absent, so
        // both of the recorder fragment's adjustments skip.
        let (flat, report) = compose(&parsed, &launcher, &words(&["recorder=on"]))
            .expect("composes");
        assert_eq!(report.skipped.len(), 2);
        assert!(report.skipped.iter().any(|s| matches!(
            s.reason,
            SkipReason::GuardNotMet(ref g) if g == "commander=xr"
        )));
        assert!(report.skipped.iter().any(|s| matches!(
            s.reason,
            SkipReason::TargetAbsent
        )));
        let backbone = flat
            .deployments
            .iter()
            .find(|d| d.source.name == "backbone")
            .unwrap();
        assert!(!backbone.instances[0].arguments.contains_key("record_backbone"));

        // xr + recorder: the guard holds and its target exists, so it
        // applies; sim_inst is still absent and still skips.
        let (flat, report) = compose(
            &parsed,
            &launcher,
            &words(&["commander=xr", "recorder=on"]),
        )
        .expect("composes");
        assert!(report
            .skipped
            .iter()
            .all(|s| matches!(s.reason, SkipReason::TargetAbsent)));
        let backbone = flat
            .deployments
            .iter()
            .find(|d| d.source.name == "backbone")
            .unwrap();
        assert_eq!(
            backbone.instances[0].arguments.get("record_backbone"),
            Some(&config::AnyType::Bool(true))
        );

        // xr + recorder + mujoco: every target exists, nothing skips.
        let (_, report) = compose(
            &parsed,
            &launcher,
            &words(&["commander=xr", "recorder=on", "robot=mujoco"]),
        )
        .expect("composes");
        assert!(report.skipped.is_empty());
    }

    #[test]
    fn an_option_missing_a_provides_id_is_refused() {
        let dir = tempdir().unwrap();
        write(&dir.path().join("fragments/bad.json5"), &fragment_file(r#"
            deployments: [
                { source: { name: "other", tag: "v1" }, instances: [{ instance_id: "wrong_inst" }] },
            ],
        "#));
        let launcher = write(&dir.path().join("l.json5"), r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "robot", provides: ["arm_inst"],
                  options: { sim: "fragments/bad.json5" } },
            ],
            deployments: [],
        }"#);
        let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
        let err = compose(&parsed, &launcher, &["sim".to_string()]).expect_err("provides unmet");
        let CompositionError::ProvidesUnmet { axis, option, id } = &err else {
            panic!("expected ProvidesUnmet, got: {err}");
        };
        assert_eq!((axis.as_str(), option.as_str(), id.as_str()), ("robot", "sim", "arm_inst"));
    }

    #[test]
    fn an_adjustment_target_defined_nowhere_is_refused() {
        let dir = tempdir().unwrap();
        write(&dir.path().join("fragments/a.json5"), &fragment_file(r#"
            adjustments: [ { target: "ghost_inst", set_arguments: { x: 1 } } ],
        "#));
        let launcher = write(&dir.path().join("l.json5"), r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "a", optional: true, options: { on: "fragments/a.json5" } },
            ],
            deployments: [],
        }"#);
        let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
        let err = compose(&parsed, &launcher, &["on".to_string()]).expect_err("dead target");
        let CompositionError::TargetDefinedNowhere { target, .. } = &err else {
            panic!("expected TargetDefinedNowhere, got: {err}");
        };
        assert_eq!(target, "ghost_inst");
    }

    #[test]
    fn unsafe_fragment_paths_are_refused() {
        for raw in ["/etc/passwd", "../outside.json5", "./here.json5", ""] {
            let dir = tempdir().unwrap();
            let spec = FragmentSpec(vec![super::super::composition::FragmentPart::File(
                raw.to_owned(),
            )]);
            let mut axis = ComponentAxis {
                name: "robot".to_owned(),
                options: BTreeMap::from([("sim".to_owned(), spec)]),
                default: None,
                optional: false,
                provides: Vec::new(),
            };
            axis.default = None;
            let launcher = PeppyLauncher {
                peppy_schema: config::schema::PeppySchema::LauncherV1,
                core_nodes: Vec::new(),
                deployments: Vec::new(),
                components: vec![axis],
                adjustments: Vec::new(),
            };
            let err = compose(&launcher, &dir.path().join("l.json5"), &["sim".to_string()])
                .expect_err("unsafe path must be refused");
            assert!(
                err.to_string().contains("not usable") || err.to_string().contains("cannot be read"),
                "raw path {raw:?}: got: {err}"
            );
        }
    }

    #[test]
    fn a_symlink_escaping_the_launcher_directory_is_refused() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        write(&outside.path().join("escape.json5"), &fragment_file("deployments: []"));
        fs::create_dir_all(dir.path().join("fragments")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("escape.json5"),
            dir.path().join("fragments/escape.json5"),
        )
        .unwrap();
        let launcher = write(&dir.path().join("l.json5"), r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "robot", optional: true,
                  options: { sim: "fragments/escape.json5" } },
            ],
            deployments: [],
        }"#);
        let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
        let err = compose(&parsed, &launcher, &["sim".to_string()]).expect_err("escape refused");
        assert!(err.to_string().contains("escapes"), "got: {err}");
    }

    #[test]
    fn core_nodes_union_base_first() {
        let dir = tempdir().unwrap();
        write(&dir.path().join("fragments/a.json5"), &fragment_file(r#"
            core_nodes: ["edge", "cloud"],
        "#));
        let launcher = write(&dir.path().join("l.json5"), r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "a", optional: true, options: { on: "fragments/a.json5" } },
            ],
            core_nodes: ["cloud", "base"],
            deployments: [],
        }"#);
        let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
        let (flat, _) = compose(&parsed, &launcher, &["on".to_string()]).expect("composes");
        assert_eq!(flat.core_nodes, ["cloud", "base", "edge"]);
    }

    // -- selection resolution ------------------------------------------------

    fn axes_from(body: &str) -> Vec<ComponentAxis> {
        serde_json5::from_str(body).expect("axes parse")
    }

    #[test]
    fn an_unknown_word_lists_every_axis_with_its_options() {
        let axes = axes_from(
            r#"[{ name: "robot", options: { real: { deployments: [] }, mujoco: { deployments: [] } } },
                { name: "commander", options: { web: { deployments: [] } } }]"#,
        );
        let err = resolve_selection(&axes, &["isaac".to_string()]).expect_err("unknown word");
        let CompositionError::UnknownSelection { menu, .. } = &err else {
            panic!("expected UnknownSelection, got: {err}");
        };
        assert!(menu.contains("robot"), "got: {menu}");
        assert!(menu.contains("`mujoco`, `real`"), "got: {menu}");
        assert!(menu.contains("commander"), "got: {menu}");
    }

    #[test]
    fn an_ambiguous_bare_word_names_the_axes() {
        let axes = axes_from(
            r#"[{ name: "robot", default: "none", options: { none: { deployments: [] } } },
                { name: "recorder", default: "none", options: { none: { deployments: [] } } }]"#,
        );
        let err = resolve_selection(&axes, &["none".to_string()]).expect_err("ambiguous word");
        let CompositionError::AmbiguousSelection { axes: named, .. } = &err else {
            panic!("expected AmbiguousSelection, got: {err}");
        };
        assert!(named.contains("`robot`") && named.contains("`recorder`"), "got: {named}");
        // The explicit form resolves.
        resolve_selection(&axes, &["robot=none".to_string()]).expect("explicit form");
    }

    #[test]
    fn two_different_options_on_one_axis_are_refused() {
        let axes = axes_from(
            r#"[{ name: "robot", options: { real: { deployments: [] }, mujoco: { deployments: [] } } }]"#,
        );
        let err = resolve_selection(
            &axes,
            &["mujoco".to_string(), "robot=real".to_string()],
        )
        .expect_err("conflicting selection");
        let CompositionError::ConflictingSelection { axis, first, second } = &err else {
            panic!("expected ConflictingSelection, got: {err}");
        };
        assert_eq!((axis.as_str(), first.as_str(), second.as_str()), ("robot", "mujoco", "real"));
        // Repeating the same option is the same choice, not a conflict.
        resolve_selection(
            &axes,
            &["mujoco".to_string(), "robot=mujoco".to_string()],
        )
        .expect("same option twice is one choice");
    }

    #[test]
    fn a_required_axis_with_no_default_refuses() {
        let axes = axes_from(
            r#"[{ name: "robot", options: { real: { deployments: [] }, mujoco: { deployments: [] } } }]"#,
        );
        let err = resolve_selection(&axes, &[]).expect_err("unresolved axis");
        let CompositionError::UnresolvedAxis { axis, options, .. } = &err else {
            panic!("expected UnresolvedAxis, got: {err}");
        };
        assert_eq!(axis, "robot");
        assert!(options.contains("`real`"), "got: {options}");
    }

    #[test]
    fn the_selection_space_enumerates_options_then_unfilled() {
        let axes = axes_from(
            r#"[
                { name: "robot", options: { real: {}, sim: {} } },
                { name: "recorder", optional: true, options: { on: {} } },
            ]"#,
        );
        let selections = enumerate_selections(&axes);
        assert_eq!(selections.len(), 4);
        assert_eq!(selection_space_size(&axes), 4);
        // Fixed order: options in name order, unfilled last on each axis.
        assert_eq!(selections[0].echo(), "robot=real  recorder=on");
        assert_eq!(selections[1].echo(), "robot=real  recorder=(off)");
        assert_eq!(selections[3].echo(), "robot=sim  recorder=(off)");
    }

    #[test]
    fn with_on_a_flat_launcher_is_refused() {
        let launcher = parse_launcher(
            r#"{ peppy_schema: "launcher/v1", deployments: [] }"#,
        );
        let err = compose(&launcher, Path::new("/tmp/x.json5"), &["mujoco".to_string()])
            .expect_err("flat launcher has nothing to select");
        assert!(matches!(err, CompositionError::WithOnFlatLauncher));
    }
}
