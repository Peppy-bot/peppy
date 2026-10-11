//! Selection resolution: the `--with` words and the file's deployments
//! against the launcher's own axes and the axes of the options they select,
//! and a copy's `with` against its option's axes.

use super::super::composition::ComponentAxis;
use super::super::types::PeppyLauncher;
use super::error::{CompositionError, UnresolvedCopyAxis};
use super::load::{LoadedComposition, LoadedOption};
use config::runtime::{CopyName, CoreNodeTarget, Name};
use core_node_api::encoding::{
    AxisSelection, CopyReference, LaunchWord, OptionChoice, SelectionWord,
};
use std::collections::BTreeMap;
use std::fmt;

/// The machine of each placed part of one copy: the own axis that brings
/// the part, and where its instances run.
pub type PartMachines = BTreeMap<Name, CoreNodeTarget>;

/// A word as its caller wrote it, for the refusals that quote it: a stack
/// word of a launch, or a selection word of one copy, written after the
/// copy's name on a launch and alone on `peppy stack join`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WrittenWord {
    Stack(OptionChoice),
    Copy {
        launch_copy: Option<CopyName>,
        word: SelectionWord,
    },
}

impl fmt::Display for WrittenWord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stack(choice) => write!(f, "{choice}"),
            Self::Copy {
                launch_copy: Some(copy),
                word,
            } => write!(f, "{copy}.{word}"),
            Self::Copy {
                launch_copy: None,
                word,
            } => write!(f, "{word}"),
        }
    }
}

/// Where the words of one copy are written: on a launch, after the copy's
/// name, or on `peppy stack join`, alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WordOrigin {
    Launch,
    Join,
}

impl WordOrigin {
    fn written(self, copy: &CopyName, word: &SelectionWord) -> Box<WrittenWord> {
        Box::new(WrittenWord::Copy {
            launch_copy: (self == Self::Launch).then(|| copy.clone()),
            word: word.clone(),
        })
    }
}

/// How one axis of a resolved selection came to be filled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionSource {
    /// Named by `--with`, or by a copy's `with`.
    Explicit,
    /// Deployed by the file: a `deployments` entry of the launcher or of
    /// the option's fragment.
    Deployed,
    /// An axis that may stay unfilled, left unfilled; contributes nothing.
    Unfilled,
}

/// One axis's fate in a resolved selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionEntry {
    pub axis: String,
    pub option: Option<String>,
    pub source: SelectionSource,
}

/// The resolved axes of one unit of a launch: the stack, or one copy. Holds
/// the launcher's own axes and the axes of the selected options, one entry
/// per axis, in declaration order. A copy's selection leads with the entry
/// of the axis it copies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnitSelection {
    pub entries: Vec<SelectionEntry>,
}

impl UnitSelection {
    pub(super) fn options_on<'a>(&'a self, axis: &'a str) -> impl Iterator<Item = &'a str> {
        self.entries
            .iter()
            .filter(move |entry| entry.axis == axis)
            .filter_map(|entry| entry.option.as_deref())
    }

    /// The option filling `axis`, if the axis is in this selection and filled.
    pub(super) fn option_of(&self, axis: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|entry| entry.axis == axis)
            .and_then(|entry| entry.option.as_deref())
    }

    /// The entries on the launcher's own axes: what a copy shares with the
    /// stack it joins.
    pub(super) fn launcher_entries(&self, launcher: &PeppyLauncher) -> Vec<SelectionEntry> {
        self.entries
            .iter()
            .filter(|entry| {
                launcher
                    .components
                    .iter()
                    .any(|axis| axis.name == entry.axis)
            })
            .cloned()
            .collect()
    }

    /// A copy's entries on its option's own axes: everything but the entry
    /// of the axis it copies.
    pub fn own_axes<'a>(
        &'a self,
        parent_axis: &'a str,
    ) -> impl Iterator<Item = &'a SelectionEntry> {
        self.entries
            .iter()
            .filter(move |entry| entry.axis != parent_axis)
    }

    /// A copy's filled own axes, in the order its option declares them, as
    /// the selections a listing gives, each with no machine.
    pub fn axis_selections(&self, parent_axis: &str) -> Vec<AxisSelection> {
        self.own_axes(parent_axis)
            .filter_map(|entry| {
                let option = entry.option.as_ref()?;
                Some(AxisSelection {
                    axis: Name::new(entry.axis.as_str()).expect("an axis name is a name"),
                    option: Name::new(option.as_str()).expect("an option name is a name"),
                    core_node: None,
                })
            })
            .collect()
    }

    /// The options a copy's selection picks on its option's own axes, as
    /// the copy's `with` map: what recomposes the copy the selection
    /// describes.
    pub(super) fn own_choices(&self, parent_axis: &str) -> BTreeMap<String, String> {
        self.own_axes(parent_axis)
            .filter_map(|entry| {
                let option = entry.option.as_ref()?;
                Some((entry.axis.clone(), option.clone()))
            })
            .collect()
    }

    /// The one-line echo of the resolution, as launch feedback prints it
    /// before anything runs: `simulation=waldo (from file)  commander=xr
    /// recorder=(off)`.
    pub fn echo(&self) -> String {
        self.placed_echo(&PartMachines::new())
    }

    /// The echo of a copy's resolution, with the machine of each placed part
    /// after its axis, as the selection word that places it writes it:
    /// `robot=openarm_sim  brain=ai_brain_vla@cn-gpu-box`.
    pub fn placed_echo(&self, parts: &PartMachines) -> String {
        self.entries
            .iter()
            .map(|entry| {
                let placed = parts
                    .iter()
                    .find(|(axis, _)| axis.as_str() == entry.axis)
                    .map_or_else(String::new, |(_, machine)| format!("@{machine}"));
                match (&entry.option, entry.source) {
                    (Some(option), SelectionSource::Deployed) => {
                        format!("{}={option}{placed} (from file)", entry.axis)
                    }
                    (Some(option), _) => format!("{}={option}{placed}", entry.axis),
                    (None, _) => format!("{}=(off)", entry.axis),
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
    }
}

/// One axis a selection can fill, with the option that declares it.
struct AxisInReach<'a> {
    axis: &'a ComponentAxis,
    parent: Parent<'a>,
}

#[derive(Clone, Copy)]
struct Parent<'a> {
    axis: &'a str,
    option: &'a str,
    loaded: &'a LoadedOption,
}

impl Parent<'_> {
    fn label(&self) -> String {
        format!("`{}={}`", self.axis, self.option)
    }
}

/// The axes among `candidates` a word names: by axis name for the
/// `axis=option` form, by option name for a bare word.
enum Match<'a> {
    Nothing,
    One(&'a ComponentAxis),
    Several(Vec<&'a str>),
}

fn match_word<'a>(
    word: &OptionChoice,
    candidates: impl Iterator<Item = &'a ComponentAxis>,
) -> Match<'a> {
    let held_by: Vec<&ComponentAxis> = match &word.axis {
        Some(axis_name) => candidates
            .filter(|axis| axis.name == axis_name.as_str())
            .collect(),
        None => candidates
            .filter(|axis| axis.options.contains_key(word.option.as_str()))
            .collect(),
    };
    match held_by.as_slice() {
        [] => Match::Nothing,
        [axis] => Match::One(axis),
        many => Match::Several(many.iter().map(|axis| axis.name.as_str()).collect()),
    }
}

/// A launch's `--with` words, split by what they name.
pub(super) struct LaunchWords {
    /// By copy, the file's or a `--join` one: the words of the copy's own
    /// axes, `NAME.axis=option` or `NAME.option`, each with the machine of
    /// the part it selects.
    pub scoped: BTreeMap<CopyName, Vec<SelectionWord>>,
    /// The words naming the launcher's axes.
    pub stack: Vec<OptionChoice>,
}

/// Splits a launch's `--with` words into the copies' and the stack's.
pub(super) fn split_scoped_words(
    launcher: &PeppyLauncher,
    words: &[LaunchWord],
    joins: &[CopyReference],
) -> Result<LaunchWords, CompositionError> {
    let copies: Vec<&CopyName> = launcher
        .option_deployments
        .iter()
        .flat_map(|entry| &entry.instances)
        .map(|copy| &copy.instance_id)
        .chain(joins.iter().map(|join| &join.name))
        .collect();
    let mut scoped: BTreeMap<CopyName, Vec<SelectionWord>> = BTreeMap::new();
    let mut stack = Vec::new();
    for word in words {
        let (copy, selection) = match word {
            LaunchWord::Stack(choice) => {
                stack.push(choice.clone());
                continue;
            }
            LaunchWord::Copy { copy, word } => (copy, word),
        };
        if !copies.contains(&copy) {
            let join = format!("`peppy stack join OPTION:{copy} --with ...` adds a copy");
            return Err(CompositionError::ScopedSelectionUnknownCopy {
                word: Box::new(word.clone()),
                copy: copy.to_string(),
                copies: if launcher.repeatable_axes().next().is_none() {
                    String::from("this launcher declares no axis running as copies")
                } else if copies.is_empty() {
                    format!(
                        "the launch starts no copy; `--join OPTION:{copy}` starts one with it, \
                         and {join}"
                    )
                } else {
                    format!(
                        "the launch starts {}; a launch word selects a copy's own axis as \
                         `NAME.axis=option` or `NAME.option`, and {join}",
                        crate::error::format_quoted_list(copies.iter())
                    )
                },
            });
        }
        scoped
            .entry(copy.clone())
            .or_default()
            .push(selection.clone());
    }
    Ok(LaunchWords { scoped, stack })
}

/// The `--with` words and the file's deployments over the launcher's own
/// axes and the axes of the options they select.
pub(super) fn resolve_stack(
    launcher: &PeppyLauncher,
    loaded: &LoadedComposition,
    words: &[OptionChoice],
) -> Result<UnitSelection, CompositionError> {
    // 1. The words that name a launcher axis. A word naming an axis that
    // runs as copies is refused here, before anything else, since the
    // command line cannot name a copy.
    let mut chosen = Chosen::default();
    let mut deferred: Vec<&OptionChoice> = Vec::new();
    for word in words {
        let option = word.option.as_str();
        match match_word(word, launcher.components.iter()) {
            Match::Nothing => deferred.push(word),
            Match::One(axis) if axis.cardinality.is_repeatable() => {
                return Err(if axis.options.contains_key(option) {
                    CompositionError::RepeatableAxisAtLaunch {
                        word: word.clone(),
                        axis: axis.name.clone(),
                        option: option.to_owned(),
                    }
                } else {
                    CompositionError::RepeatableAxisUnknownOption {
                        word: word.clone(),
                        axis: axis.name.clone(),
                        menu: axes_menu(std::iter::once(axis)),
                    }
                });
            }
            Match::One(axis) => {
                if !axis.options.contains_key(option) {
                    return Err(unknown_selection(word, launcher, loaded, &[]));
                }
                chosen.choose(
                    &axis.name,
                    option,
                    Box::new(WrittenWord::Stack(word.clone())),
                )?;
            }
            Match::Several(axes) => {
                return Err(ambiguous(Box::new(WrittenWord::Stack(word.clone())), &axes));
            }
        }
    }

    // 2. The launcher's own axes: the word, else the file's entry, else
    // unfilled where the cardinality allows.
    let mut entries = Vec::new();
    for axis in launcher.stack_axes() {
        entries.push(fill(
            axis,
            chosen.option_of(&axis.name),
            launcher.deployed_option(&axis.name),
            Owner::Launcher,
        )?);
    }

    // 3. The axes of the selected options, each reachable once.
    let reach = reach_of(launcher, loaded, &entries)?;
    let mut nested_chosen = Chosen::default();
    for word in deferred {
        let option = word.option.as_str();
        match match_word(word, reach.iter().map(|item| item.axis)) {
            Match::One(axis) if axis.options.contains_key(option) => {
                nested_chosen.choose(
                    &axis.name,
                    option,
                    Box::new(WrittenWord::Stack(word.clone())),
                )?;
            }
            Match::One(_) | Match::Nothing => {
                return Err(unknown_selection(word, launcher, loaded, &reach));
            }
            Match::Several(axes) => {
                return Err(ambiguous(Box::new(WrittenWord::Stack(word.clone())), &axes));
            }
        }
    }
    let mut nested = Vec::with_capacity(reach.len());
    for item in &reach {
        nested.push(fill(
            item.axis,
            nested_chosen.option_of(&item.axis.name),
            item.parent
                .loaded
                .deployed
                .get(&item.axis.name)
                .map(String::as_str),
            Owner::Option(item.parent.option),
        )?);
    }
    entries.extend(nested);
    Ok(UnitSelection { entries })
}

/// The axes of every option `entries` select, each declared by exactly one
/// of them.
pub(super) fn check_reach(
    launcher: &PeppyLauncher,
    loaded: &LoadedComposition,
    entries: &[SelectionEntry],
) -> Result<(), CompositionError> {
    reach_of(launcher, loaded, entries).map(|_| ())
}

fn reach_of<'a>(
    launcher: &'a PeppyLauncher,
    loaded: &'a LoadedComposition,
    entries: &'a [SelectionEntry],
) -> Result<Vec<AxisInReach<'a>>, CompositionError> {
    let mut reach: Vec<AxisInReach<'a>> = Vec::new();
    for entry in entries {
        let Some(option) = &entry.option else {
            continue;
        };
        if !launcher
            .components
            .iter()
            .any(|axis| axis.name == entry.axis)
        {
            continue;
        }
        let loaded_option = loaded.option(&entry.axis, option);
        let parent = Parent {
            axis: &entry.axis,
            option: loaded_option.name.as_str(),
            loaded: loaded_option,
        };
        for axis in &loaded_option.axes {
            if let Some(holder) = reach.iter().find(|item| item.axis.name == axis.name) {
                return Err(CompositionError::AxisInReachTwice {
                    axis: axis.name.clone(),
                    first: holder.parent.label(),
                    second: parent.label(),
                });
            }
            reach.push(AxisInReach { axis, parent });
        }
    }
    Ok(reach)
}

/// The option each axis is chosen with, and the word that chose it.
#[derive(Default)]
struct Chosen {
    by_axis: BTreeMap<String, (String, Box<WrittenWord>)>,
}

impl Chosen {
    /// Chooses `option` on `axis` by `word`; a second word that chooses
    /// another option on the axis is refused.
    fn choose(
        &mut self,
        axis: &str,
        option: &str,
        word: Box<WrittenWord>,
    ) -> Result<(), CompositionError> {
        if let Some((first_option, first)) = self.by_axis.get(axis)
            && first_option != option
        {
            return Err(CompositionError::ConflictingSelection {
                axis: axis.to_owned(),
                first: first.clone(),
                second: word,
            });
        }
        self.by_axis
            .entry(axis.to_owned())
            .or_insert((option.to_owned(), word));
        Ok(())
    }

    fn option_of(&self, axis: &str) -> Option<&String> {
        self.by_axis.get(axis).map(|(option, _)| option)
    }
}

/// Who names a copy's selection: the file's `deployments` entry, whose
/// `one` axes a launch word may fill, or a `stack join`, whose words fill
/// them.
#[derive(Clone, Copy)]
pub(super) enum CopyOrigin {
    File,
    /// Named at launch with `--join OPTION:NAME`: the launcher's entry for
    /// the option is where its settings are written.
    LaunchJoin,
    Join,
}

/// Who an axis belongs to, for the refusal when nothing fills it.
#[derive(Clone, Copy)]
enum Owner<'a> {
    Launcher,
    Option(&'a str),
    Copy {
        copy: &'a str,
        option: &'a str,
        origin: CopyOrigin,
    },
}

/// One axis's entry: the explicit choice, else what the file deploys, else
/// unfilled where the cardinality allows it.
fn fill(
    axis: &ComponentAxis,
    explicit: Option<&String>,
    deployed: Option<&str>,
    owner: Owner<'_>,
) -> Result<SelectionEntry, CompositionError> {
    let (option, source) = match (explicit, deployed) {
        (Some(option), _) => (Some(option.clone()), SelectionSource::Explicit),
        (None, Some(option)) => (Some(option.to_owned()), SelectionSource::Deployed),
        (None, None) if axis.cardinality.allows_empty() => (None, SelectionSource::Unfilled),
        (None, None) => {
            let options = crate::error::format_quoted_list(axis.options.keys());
            return Err(match owner {
                Owner::Launcher => CompositionError::UnresolvedAxis {
                    axis: axis.name.clone(),
                    origin: String::new(),
                    options,
                },
                Owner::Option(option) => CompositionError::UnresolvedAxis {
                    axis: axis.name.clone(),
                    origin: format!(" of `{option}`"),
                    options,
                },
                Owner::Copy {
                    copy,
                    option,
                    origin,
                } => CompositionError::UnresolvedCopyAxis(Box::new(UnresolvedCopyAxis {
                    copy: copy.to_owned(),
                    option: option.to_owned(),
                    axis: axis.name.clone(),
                    fix: match origin {
                        CopyOrigin::File => format!(
                            "add `with: {{ {}: \"<option>\" }}` to the copy, or `--with {copy}.{}=<option>` \
                             on `peppy stack launch`",
                            axis.name, axis.name
                        ),
                        CopyOrigin::LaunchJoin => format!(
                            "add `with: {{ {}: \"<option>\" }}` to the launcher's entry for \
                             `{option}`, or `--with {copy}.{}=<option>` on `peppy stack launch`",
                            axis.name, axis.name
                        ),
                        CopyOrigin::Join => String::from("`--with <option>` on `peppy stack join`"),
                    },
                    options,
                })),
            });
        }
    };
    Ok(SelectionEntry {
        axis: axis.name.clone(),
        option,
        source,
    })
}

fn ambiguous(word: Box<WrittenWord>, axes: &[&str]) -> CompositionError {
    CompositionError::AmbiguousSelection {
        word,
        axes: crate::error::format_quoted_list(axes),
    }
}

/// The refusal for a word that names nothing in reach, with the menu the
/// fix picks from: the launcher's axes and the selected options' axes. A
/// word that names an axis of a copy's option is told where copies are
/// selected instead.
fn unknown_selection(
    word: &OptionChoice,
    launcher: &PeppyLauncher,
    loaded: &LoadedComposition,
    reach: &[AxisInReach<'_>],
) -> CompositionError {
    let option = word.option.as_str();
    for parent_axis in launcher.repeatable_axes() {
        for (option_name, loaded_option) in loaded.options_of(&parent_axis.name) {
            let holder = loaded_option.axes.iter().find(|axis| match &word.axis {
                Some(axis_name) => {
                    axis.name == axis_name.as_str() && axis.options.contains_key(option)
                }
                None => axis.options.contains_key(option),
            });
            if let Some(axis) = holder {
                return CompositionError::CopyAxisAtLaunch {
                    word: Box::new(word.clone()),
                    axis: axis.name.clone(),
                    parent: option_name.clone(),
                    option: option.to_owned(),
                };
            }
        }
    }
    let mut menu: String = axes_menu(launcher.stack_axes());
    menu.push_str(&axes_menu(reach.iter().map(|item| item.axis)));
    CompositionError::UnknownSelection {
        word: word.clone(),
        menu,
    }
}

/// One line per axis naming its options, for the "names no option" refusals:
/// the fix is picking from the list, so the list is the error.
pub(super) fn axes_menu<'a>(axes: impl Iterator<Item = &'a ComponentAxis>) -> String {
    crate::error::format_bulleted(axes.map(|axis| {
        format!(
            "{}: {}",
            axis.name,
            crate::error::format_quoted_list(axis.options.keys())
        )
    }))
}

/// A copy's `with` over its option's own axes, with the copy's own axis
/// entry first.
pub(super) fn resolve_copy(
    loaded: &LoadedOption,
    parent_axis: &str,
    copy: &str,
    with: &BTreeMap<String, String>,
    origin: CopyOrigin,
) -> Result<UnitSelection, CompositionError> {
    for (axis_name, option) in with {
        let Some(axis) = loaded.axis(axis_name) else {
            return Err(CompositionError::CopySelectsUnknownAxis {
                copy: copy.to_owned(),
                option: loaded.name.clone(),
                axis: axis_name.clone(),
                axes: crate::error::format_quoted_list(
                    loaded.axes.iter().map(ComponentAxis::as_str),
                ),
            });
        };
        if !axis.options.contains_key(option) {
            return Err(CompositionError::CopySelectsUnknownOption {
                copy: copy.to_owned(),
                option: loaded.name.clone(),
                selection: format!("{axis_name}: {option}"),
                options: crate::error::format_quoted_list(axis.options.keys()),
            });
        }
    }
    let mut entries = vec![SelectionEntry {
        axis: parent_axis.to_owned(),
        option: Some(loaded.name.clone()),
        source: SelectionSource::Explicit,
    }];
    for axis in &loaded.axes {
        entries.push(fill(
            axis,
            with.get(&axis.name),
            loaded.deployed.get(&axis.name).map(String::as_str),
            Owner::Copy {
                copy,
                option: &loaded.name,
                origin,
            },
        )?);
    }
    Ok(UnitSelection { entries })
}

/// The words of one copy, resolved over the copied option's own axes: the
/// option each word chooses, as the copy's `with` map, and the machine of
/// each part a word places.
#[derive(Debug, Default)]
pub(super) struct CopyWords {
    pub with: BTreeMap<String, String>,
    pub parts: PartMachines,
}

/// The words of the copy `copy` over the copied option's own axes: each
/// word names an option of one of them, and a word with a machine places
/// the part that option brings. Two words on one axis must choose one
/// option and name one machine, as written: no machine and `@self` differ,
/// and so do `@self` and the coordinator's name, which composition cannot
/// know.
pub(super) fn copy_words(
    loaded: &LoadedOption,
    copy: &CopyName,
    origin: WordOrigin,
    words: &[SelectionWord],
) -> Result<CopyWords, CompositionError> {
    let mut chosen: BTreeMap<String, &SelectionWord> = BTreeMap::new();
    for word in words {
        let option = word.choice.option.as_str();
        let axis = match match_word(&word.choice, loaded.axes.iter()) {
            Match::One(axis) if axis.options.contains_key(option) => axis,
            Match::One(_) | Match::Nothing => {
                return Err(CompositionError::UnknownCopySelection {
                    word: origin.written(copy, word),
                    option: loaded.name.clone(),
                    menu: if loaded.axes.is_empty() {
                        String::from(" It declares none.")
                    } else {
                        axes_menu(loaded.axes.iter())
                    },
                });
            }
            Match::Several(axes) => return Err(ambiguous(origin.written(copy, word), &axes)),
        };
        if let Some(first) = chosen.get(&axis.name) {
            if first.choice.option != word.choice.option {
                return Err(CompositionError::ConflictingSelection {
                    axis: axis.name.clone(),
                    first: origin.written(copy, first),
                    second: origin.written(copy, word),
                });
            }
            if first.core_node != word.core_node {
                return Err(CompositionError::PartPlacedTwice {
                    copy: copy.to_string(),
                    axis: axis.name.clone(),
                    first: origin.written(copy, first),
                    second: origin.written(copy, word),
                });
            }
            continue;
        }
        if let Some(machine) = &word.core_node
            && !loaded.brings_instances(&axis.name, option)
        {
            return Err(CompositionError::PlacedPartBringsNothing {
                copy: copy.to_string(),
                word: origin.written(copy, word),
                axis: axis.name.clone(),
                option: option.to_owned(),
                machine: machine.to_string(),
            });
        }
        chosen.insert(axis.name.clone(), word);
    }
    let mut resolved = CopyWords::default();
    for (axis, word) in chosen {
        if let Some(machine) = &word.core_node {
            resolved.parts.insert(
                Name::new(axis.as_str()).expect("an axis name is a name"),
                machine.clone(),
            );
        }
        resolved.with.insert(axis, word.choice.option.to_string());
    }
    Ok(resolved)
}
