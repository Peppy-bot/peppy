//! Loading a composition: every fragment the launcher's options reference,
//! and the axes those fragments declare, read once and held to the
//! selection-independent checks.

use super::super::composition::{
    ComponentAxis, Fragment, FragmentPart, FragmentSpec, LauncherFragmentParser, OptionDeployment,
    validate_fragment_references,
};
use super::super::types::PeppyLauncher;
use super::constraints::{constraint_names_axis, names_axis};
use super::error::CompositionError;
use super::select::{UnitSelection, resolve_copy};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Component as PathComponent, Path, PathBuf};

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
    /// selection: its own fragments, then those of the options `own`
    /// picks on its axes.
    pub(super) fn fragments_for(&self, own: &UnitSelection) -> Vec<&LoadedFragment> {
        self.fragments
            .iter()
            .chain(self.axes.iter().flat_map(|axis| {
                own.option_of(&axis.name)
                    .map_or(&[][..], |option| self.nested_fragments(&axis.name, option))
                    .iter()
            }))
            .collect()
    }

    /// The instance ids this option's own fragments define.
    fn own_ids(&self) -> impl Iterator<Item = &str> {
        node_ids(self.fragments.iter())
    }

    /// Every instance id this option can define, its own axes' options
    /// included.
    pub(super) fn definable_ids(&self) -> HashSet<&str> {
        self.own_ids()
            .chain(
                self.nested
                    .values()
                    .flat_map(|options| options.values())
                    .flat_map(|fragments| node_ids(fragments.iter())),
            )
            .collect()
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

fn node_ids<'a>(
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
/// constraint references, adjustment targets defined somewhere, and copies
/// that select what their option declares.
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
            let origin = format!("{launcher_label}, option `{}.{}`", axis.name, option_name);
            let fragments = reader.read_parts(spec, &origin, None, &axis.name, option_name)?;
            let loaded = load_option(&mut reader, launcher, option_name, fragments)?;
            loaded_axis.insert(option_name.clone(), loaded);
        }
        options.insert(axis.name.clone(), loaded_axis);
    }
    let loaded = LoadedComposition { options };
    check_provides(launcher, &loaded)?;
    check_references(launcher, &loaded)?;
    check_targets(launcher, &loaded, &launcher_label)?;
    check_copy_axis_references(launcher, &loaded)?;
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
                    "{}, option `{}.{}`",
                    fragment.origin, axis.name, option_name
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

/// Every guard and constraint of every fragment names axes in its reach:
/// the launcher's, and the axes of the option it belongs to.
fn check_references(
    launcher: &PeppyLauncher,
    loaded: &LoadedComposition,
) -> Result<(), CompositionError> {
    for axis in &launcher.components {
        for (_, option) in loaded.options_of(&axis.name) {
            let in_reach: Vec<&ComponentAxis> = launcher
                .components
                .iter()
                .chain(option.axes.iter())
                .collect();
            for fragment in option.all_fragments() {
                validate_fragment_references(&fragment.body, &in_reach, &fragment.origin).map_err(
                    |detail| CompositionError::FragmentReferencesUnknownAxis {
                        path: fragment.origin.clone(),
                        origin: format!("option `{}.{}`", axis.name, option.name),
                        detail,
                    },
                )?;
            }
        }
    }
    Ok(())
}

/// An adjustment target the selection does not define is skipped; a target
/// nothing can define is a dead reference, refused here once for every
/// selection.
fn check_targets(
    launcher: &PeppyLauncher,
    loaded: &LoadedComposition,
    launcher_label: &str,
) -> Result<(), CompositionError> {
    let all_options: Vec<&LoadedOption> = launcher
        .components
        .iter()
        .flat_map(|axis| loaded.options_of(&axis.name))
        .map(|(_, option)| option)
        .collect();
    let mut definable: HashSet<&str> = launcher
        .deployments
        .iter()
        .flat_map(|d| d.instances.iter())
        .map(|i| i.instance_id.as_str())
        .collect();
    for option in &all_options {
        definable.extend(option.definable_ids());
    }
    for adjustment in &launcher.adjustments {
        if !definable.contains(adjustment.target.as_str()) {
            return Err(CompositionError::TargetDefinedNowhere {
                target: adjustment.target.to_string(),
                origin: format!("{launcher_label} (base)"),
            });
        }
    }
    for fragment in all_options.iter().flat_map(|option| option.all_fragments()) {
        for adjustment in &fragment.body.adjustments {
            if !definable.contains(adjustment.target.as_str()) {
                return Err(CompositionError::TargetDefinedNowhere {
                    target: adjustment.target.to_string(),
                    origin: fragment.origin.clone(),
                });
            }
        }
    }
    Ok(())
}

/// A unit's selection fills the launcher's stack axes and, for a copy, its
/// own axis: a guard or constraint naming another copy axis can never
/// hold, a launcher constraint naming two copy axes holds for neither, and
/// a copy's fragment has no core node link to declare, its name being its
/// placement. Each is refused where it is written.
fn check_copy_axis_references(
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
            }
        }
    }
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
        for copy in &entry.instances {
            let (with, arguments) = entry.settings_for(copy);
            let own = resolve_copy(loaded_option, axis, copy.instance_id.as_str(), &with)?;
            let defined: BTreeSet<&str> = loaded_option
                .fragments_for(&own)
                .into_iter()
                .flat_map(|fragment| &fragment.body.deployments)
                .flat_map(|deployment| &deployment.instances)
                .map(|instance| instance.instance_id.as_str())
                .collect();
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

/// Reads fragment files once each, resolving every path inside the
/// launcher's repository, giving each file one identity however many
/// options name it and each inline body its own.
struct FragmentReader<'a> {
    launcher_file: &'a Path,
    // Resolved at the first file reference. Inline-only compositions
    // perform no filesystem reads.
    paths: Option<FragmentPaths>,
    // Several options may reference one fragment file (the relay fragment
    // under both simulated robots); it is read and parsed once, keyed by
    // its resolved path, and keeps the identity it was given.
    files: HashMap<PathBuf, (usize, Fragment)>,
    next_id: usize,
}

impl FragmentReader<'_> {
    /// One option's parts, files read relative to `directory` (the
    /// declaring fragment's own, or the launcher's when `None`).
    fn read_parts(
        &mut self,
        spec: &FragmentSpec,
        origin: &str,
        directory: Option<&Path>,
        axis: &str,
        option: &str,
    ) -> Result<Vec<LoadedFragment>, CompositionError> {
        let mut loaded = Vec::with_capacity(spec.0.len());
        for part in &spec.0 {
            let (id, body, label, directory) = match part {
                FragmentPart::Inline(fragment) => (
                    self.fresh_id(),
                    fragment.clone(),
                    format!("inline option `{axis}.{option}`"),
                    directory.map(Path::to_path_buf),
                ),
                FragmentPart::File(raw) => {
                    let (id, body, path) = self.read_file(raw, origin, directory)?;
                    let label = self.label_of(&path);
                    (id, body, label, path.parent().map(Path::to_path_buf))
                }
            };
            loaded.push(LoadedFragment {
                id,
                body,
                origin: label,
                directory,
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
            self.paths = Some(FragmentPaths::for_launcher(self.launcher_file)?);
        }
        let paths = self.paths.as_ref().expect("resolved just above");
        let base = directory.unwrap_or(&paths.directory);
        let path = resolve_fragment_path(paths, base, raw, origin)?;
        if let Some((id, cached)) = self.files.get(&path) {
            return Ok((*id, cached.clone(), path));
        }
        let body = read_fragment_file(&path, raw, origin)?;
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
/// fragment parser (which owns the read-and-refuse-empty half).
fn read_fragment_file(path: &Path, raw: &str, origin: &str) -> Result<Fragment, CompositionError> {
    let parsed = LauncherFragmentParser::from_path(path).map_err(|e| match &e {
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

/// Canonical launch directory and the nearest enclosing Peppy repository.
/// A standalone launcher's repository is its own directory.
struct FragmentPaths {
    directory: PathBuf,
    boundary: PathBuf,
}

impl FragmentPaths {
    fn for_launcher(file: &Path) -> Result<Self, CompositionError> {
        let directory = launcher_dir_of(file).canonicalize().map_err(|error| {
            CompositionError::LauncherDirectory {
                reason: format!(
                    "{} cannot be resolved: {error}",
                    launcher_dir_of(file).display()
                ),
            }
        })?;
        for ancestor in directory.ancestors() {
            let index = ancestor.join(crate::consts::REPOSITORY_INDEX_FILE);
            match index.try_exists() {
                Ok(false) => continue,
                Err(error) => {
                    return Err(CompositionError::LauncherDirectory {
                        reason: format!("{} cannot be read: {error}", index.display()),
                    });
                }
                Ok(true) => {}
            }
            crate::repository::PeppyRepositoryIndexParser::from_path(&index).map_err(|error| {
                CompositionError::LauncherDirectory {
                    reason: format!("{} does not parse: {error}", index.display()),
                }
            })?;
            return Ok(Self {
                boundary: ancestor.to_path_buf(),
                directory,
            });
        }
        Ok(Self {
            boundary: directory.clone(),
            directory,
        })
    }
}

/// The directory fragment paths resolve against: the launcher's own, or the
/// working directory when the launcher path itself was elided.
fn launcher_dir_of(launcher_file: &Path) -> PathBuf {
    match launcher_file.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Paths are relative to the declaring document's directory. Both the
/// written path and the resolved symlink target must stay inside the
/// launcher's repository.
fn resolve_fragment_path(
    paths: &FragmentPaths,
    directory: &Path,
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
    let mut cleaned = directory.to_path_buf();
    for component in relative.components() {
        match component {
            PathComponent::Normal(segment) => cleaned.push(segment),
            PathComponent::CurDir => return refuse("it contains a `.` segment"),
            PathComponent::ParentDir => {
                if cleaned == paths.boundary {
                    return refuse("it leaves the launcher's repository");
                }
                cleaned.pop();
            }
            _ => return refuse("it is not a plain relative path"),
        }
    }
    let full = directory.join(relative);
    let canonical = full
        .canonicalize()
        .map_err(|e| CompositionError::FragmentUnreadable {
            path: raw.to_owned(),
            origin: origin.to_owned(),
            detail: e.to_string(),
        })?;
    if !canonical.starts_with(&paths.boundary) {
        return refuse("it leaves the launcher's repository through a symlink");
    }
    Ok(canonical)
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
