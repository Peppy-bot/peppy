//! Selection resolution: the `--with` words and the file's deployments
//! against the launcher's own axes and the axes of the options they select,
//! and a copy's `with` against its option's axes.

use super::super::composition::ComponentAxis;
use super::super::types::PeppyLauncher;
use super::error::CompositionError;
use super::load::{LoadedComposition, LoadedOption};
use std::collections::BTreeMap;

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

    /// The one-line echo of the resolution, as launch feedback prints it
    /// before anything runs: `simulation=waldo (from file)  commander=xr
    /// recorder=(off)`.
    pub fn echo(&self) -> String {
        self.entries
            .iter()
            .map(|entry| match (&entry.option, entry.source) {
                (Some(option), SelectionSource::Deployed) => {
                    format!("{}={option} (from file)", entry.axis)
                }
                (Some(option), _) => format!("{}={option}", entry.axis),
                (None, _) => format!("{}=(off)", entry.axis),
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

/// One `--with` word, split into the form it was written in.
fn split_word(word: &str) -> (Option<&str>, &str) {
    match word.split_once('=') {
        Some((axis, option)) => (Some(axis), option),
        None => (None, word),
    }
}

/// The axes among `candidates` a word names: by axis name for the
/// `axis=option` form, by option name for a bare word.
enum Match<'a> {
    Nothing,
    One(&'a ComponentAxis),
    Several(Vec<&'a str>),
}

fn match_word<'a>(word: &str, candidates: impl Iterator<Item = &'a ComponentAxis>) -> Match<'a> {
    let (axis_name, option) = split_word(word);
    let held_by: Vec<&ComponentAxis> = match axis_name {
        Some(axis_name) => candidates.filter(|axis| axis.name == axis_name).collect(),
        None => candidates
            .filter(|axis| axis.options.contains_key(option))
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
    /// By file copy: the parts after the copy's name of the words scoping
    /// that copy's own axes, `NAME.axis=option` or `NAME.option`.
    pub scoped: BTreeMap<String, Vec<String>>,
    /// The words naming the launcher's axes.
    pub stack: Vec<String>,
}

/// Splits a launch's `--with` words into the file copies' and the stack's.
pub(super) fn split_scoped_words(
    launcher: &PeppyLauncher,
    words: &[String],
) -> Result<LaunchWords, CompositionError> {
    let copies: Vec<&str> = launcher
        .option_deployments
        .iter()
        .flat_map(|entry| &entry.instances)
        .map(|copy| copy.instance_id.as_str())
        .collect();
    let mut scoped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut stack = Vec::new();
    for word in words {
        let selector = word.split_once('=').map_or(word.as_str(), |(head, _)| head);
        let Some((copy, tail)) = selector.split_once('.') else {
            stack.push(word.clone());
            continue;
        };
        if copy.is_empty() {
            return Err(CompositionError::ScopedWordNamesNoCopy { word: word.clone() });
        }
        if tail.is_empty() {
            return Err(CompositionError::ScopedWordNamesNoOption {
                word: word.clone(),
                copy: copy.to_owned(),
            });
        }
        if !copies.contains(&copy) {
            let join = format!("`peppy stack join OPTION -i {copy} --with ...` adds a copy");
            return Err(CompositionError::ScopedSelectionUnknownCopy {
                word: word.clone(),
                copy: copy.to_owned(),
                copies: if launcher.repeatable_axes().next().is_none() {
                    String::from("this launcher declares no axis running as copies")
                } else if copies.is_empty() {
                    format!("the file deploys no copies, and {join}")
                } else {
                    format!(
                        "the file deploys {}; a launch word selects a file copy's own axis as \
                         `NAME.axis=option` or `NAME.option`, and {join}",
                        crate::error::format_quoted_list(copies.iter().copied())
                    )
                },
            });
        }
        scoped
            .entry(copy.to_owned())
            .or_default()
            .push(word[copy.len() + 1..].to_owned());
    }
    Ok(LaunchWords { scoped, stack })
}

/// The `--with` words and the file's deployments over the launcher's own
/// axes and the axes of the options they select.
pub(super) fn resolve_stack(
    launcher: &PeppyLauncher,
    loaded: &LoadedComposition,
    words: &[String],
) -> Result<UnitSelection, CompositionError> {
    // 1. The words that name a launcher axis. A word naming an axis that
    // runs as copies is refused here, before anything else, since the
    // command line cannot name a copy.
    let mut chosen: BTreeMap<String, String> = BTreeMap::new();
    let mut deferred: Vec<&str> = Vec::new();
    for word in words {
        let (_, option) = split_word(word);
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
                choose(&mut chosen, &axis.name, option)?;
            }
            Match::Several(axes) => return Err(ambiguous(word, &axes)),
        }
    }

    // 2. The launcher's own axes: the word, else the file's entry, else
    // unfilled where the cardinality allows.
    let mut entries = Vec::new();
    for axis in launcher.stack_axes() {
        entries.push(fill(
            axis,
            chosen.get(&axis.name),
            launcher.deployed_option(&axis.name),
            Owner::Launcher,
        )?);
    }

    // 3. The axes of the selected options, each reachable once.
    let reach = reach_of(launcher, loaded, &entries)?;
    let mut nested_chosen: BTreeMap<String, String> = BTreeMap::new();
    for word in deferred {
        let (_, option) = split_word(word);
        match match_word(word, reach.iter().map(|item| item.axis)) {
            Match::One(axis) if axis.options.contains_key(option) => {
                choose(&mut nested_chosen, &axis.name, option)?;
            }
            Match::One(_) | Match::Nothing => {
                return Err(unknown_selection(word, launcher, loaded, &reach));
            }
            Match::Several(axes) => return Err(ambiguous(word, &axes)),
        }
    }
    let mut nested = Vec::with_capacity(reach.len());
    for item in &reach {
        nested.push(fill(
            item.axis,
            nested_chosen.get(&item.axis.name),
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

fn choose(
    chosen: &mut BTreeMap<String, String>,
    axis: &str,
    option: &str,
) -> Result<(), CompositionError> {
    if let Some(first) = chosen.get(axis)
        && first != option
    {
        return Err(CompositionError::ConflictingSelection {
            axis: axis.to_owned(),
            first: first.clone(),
            second: option.to_owned(),
        });
    }
    chosen.insert(axis.to_owned(), option.to_owned());
    Ok(())
}

/// Who an axis belongs to, for the refusal when nothing fills it.
#[derive(Clone, Copy)]
enum Owner<'a> {
    Launcher,
    Option(&'a str),
    Copy { copy: &'a str, option: &'a str },
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
                Owner::Copy { copy, option } => CompositionError::UnresolvedCopyAxis {
                    copy: copy.to_owned(),
                    option: option.to_owned(),
                    axis: axis.name.clone(),
                    options,
                },
            });
        }
    };
    Ok(SelectionEntry {
        axis: axis.name.clone(),
        option,
        source,
    })
}

fn ambiguous(word: &str, axes: &[&str]) -> CompositionError {
    CompositionError::AmbiguousSelection {
        word: word.to_owned(),
        axes: axes
            .iter()
            .map(|axis| format!("`{axis}`"))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// The refusal for a word that names nothing in reach, with the menu the
/// fix picks from: the launcher's axes and the selected options' axes. A
/// word that names an axis of a copy's option is told where copies are
/// selected instead.
fn unknown_selection(
    word: &str,
    launcher: &PeppyLauncher,
    loaded: &LoadedComposition,
    reach: &[AxisInReach<'_>],
) -> CompositionError {
    let (axis_name, option) = split_word(word);
    for parent_axis in launcher.repeatable_axes() {
        for (option_name, loaded_option) in loaded.options_of(&parent_axis.name) {
            let holder = loaded_option.axes.iter().find(|axis| match axis_name {
                Some(axis_name) => axis.name == axis_name && axis.options.contains_key(option),
                None => axis.options.contains_key(option),
            });
            if let Some(axis) = holder {
                return CompositionError::CopyAxisAtLaunch {
                    word: word.to_owned(),
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
        word: word.to_owned(),
        menu,
    }
}

/// One line per axis naming its options, for the "names no option" refusals:
/// the fix is picking from the list, so the list is the error.
pub(super) fn axes_menu<'a>(axes: impl Iterator<Item = &'a ComponentAxis>) -> String {
    axes.map(|axis| {
        format!(
            "\n  - {}: {}",
            axis.name,
            crate::error::format_quoted_list(axis.options.keys())
        )
    })
    .collect()
}

/// A copy's `with` over its option's own axes, with the copy's own axis
/// entry first.
pub(super) fn resolve_copy(
    loaded: &LoadedOption,
    parent_axis: &str,
    copy: &str,
    with: &BTreeMap<String, String>,
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
            },
        )?);
    }
    Ok(UnitSelection { entries })
}

/// A join's `--with` words as a copy's `with` map: each word names an
/// option of one of the copied option's own axes.
pub(super) fn copy_words(
    loaded: &LoadedOption,
    words: &[String],
) -> Result<BTreeMap<String, String>, CompositionError> {
    let mut chosen = BTreeMap::new();
    for word in words {
        let (_, option) = split_word(word);
        match match_word(word, loaded.axes.iter()) {
            Match::One(axis) if axis.options.contains_key(option) => {
                choose(&mut chosen, &axis.name, option)?;
            }
            Match::One(_) | Match::Nothing => {
                return Err(CompositionError::UnknownCopySelection {
                    word: word.clone(),
                    option: loaded.name.clone(),
                    menu: if loaded.axes.is_empty() {
                        String::from(" It declares none.")
                    } else {
                        axes_menu(loaded.axes.iter())
                    },
                });
            }
            Match::Several(axes) => return Err(ambiguous(word, &axes)),
        }
    }
    Ok(chosen)
}
