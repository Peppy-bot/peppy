//! `repo index --check`'s view of a composed launcher: every legal
//! selection of its axes, and every copy it can run, held to the same
//! checks a launch would run.

use super::super::composition::ComponentAxis;
use super::super::types::PeppyLauncher;
use super::constraints::{
    ConstraintInPlay, ConstraintScope, LAUNCHER, constraint_satisfied, constraints_in_play,
    render_constraint,
};
use super::copy::{CopyRequest, attach, combine, compose_copy};
use super::error::CompositionError;
use super::load::{LoadedComposition, LoadedOption, launcher_file_label};
use super::prepared::PreparedLauncher;
use super::select::{SelectionEntry, SelectionSource, UnitSelection, check_reach};
use config::runtime::Name;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// The ceiling on the selection space [`check_composition`] enumerates.
/// Above it the bare launch and the per-axis validation run (every fragment
/// exists, parses, provides, and guards cleanly), and the skipped
/// cross-combination checks are reported as a problem: a check that skips
/// combinations must fail or it looks like it checked them all.
const COMBINATION_CEILING: usize = 2048;

/// The name every checked copy runs under.
const CHECK_COPY: &str = "composition_check";

/// One option of a repeatable axis, as the check enumerates copies of it.
struct Repeatable<'a> {
    axis: &'a ComponentAxis,
    option: &'a str,
    loaded: &'a LoadedOption,
}

/// Holds a composed launcher to the same checks a launch would run, across
/// every legal selection of its axes and every copy of its repeatable
/// options: the wholesale validation `repo index --check` performs per
/// launcher. All of it is launcher-local (no node manifests, no daemon),
/// which is what lets it run on a contributor's branch: the commit that
/// adds an option whose shape contradicts an existing adjustment is the
/// commit whose check fails.
///
/// A legal selection is one the constraints admit: refused selections are
/// refused by design and are not required to compose, but the constraints
/// themselves are held to leaving a usable family behind (no dead option,
/// no dead constraint, a bare launch that still starts).
///
/// Returns the rendered problems, each prefixed with the launcher's file
/// name; an empty vec means every legal selection composes and the
/// constraints refuse only what the author could mean to refuse.
pub fn check_composition(launcher: &PeppyLauncher, launcher_file: &Path) -> Vec<String> {
    let label = launcher_file_label(launcher_file);
    let prepared = match PreparedLauncher::load(launcher, launcher_file) {
        Ok(prepared) => prepared,
        Err(e) => return vec![format!("{label}: {e}")],
    };
    let mut problems = check_bare_launch(&prepared, &label);
    let repeatable: Vec<Repeatable> = launcher
        .repeatable_axes()
        .flat_map(|axis| {
            prepared
                .loaded
                .options_of(&axis.name)
                .map(move |(option, loaded)| Repeatable {
                    axis,
                    option,
                    loaded,
                })
        })
        .collect();
    if let Some(problem) = above_ceiling(&prepared, &repeatable, &label) {
        problems.push(problem);
        return problems;
    }
    let mut ledger = Ledger::new(&prepared);
    let stacks = legal_stacks(&prepared, &mut ledger, &label, &mut problems);
    problems.extend(check_file_copies_over(&prepared, &stacks, &label));
    let legal_copies = check_copies_over(
        &prepared,
        &repeatable,
        &stacks,
        &mut ledger,
        &label,
        &mut problems,
    );
    problems.extend(ledger.dead_constraints(&label));
    problems.extend(dead_states(
        &prepared,
        &stacks,
        &repeatable,
        &legal_copies,
        &label,
    ));
    problems
}

/// The bare launch must stay a member of the family: what the file
/// deploys, with nothing selected on top, has to compose. A `one` axis the
/// file leaves for `--with` has no bare launch, and that is the author's
/// call.
fn check_bare_launch(prepared: &PreparedLauncher, label: &str) -> Vec<String> {
    match prepared.launch(&[]) {
        Ok(_) | Err(CompositionError::UnresolvedAxis { .. }) => Vec::new(),
        Err(e) => vec![format!(
            "{label}: the bare launch (no `--with`) fails: {e}. A launch of what the file \
             deploys must start"
        )],
    }
}

/// Per-axis validation ran when the composition loaded; above the ceiling
/// the cross-axial combinations are what goes unchecked, and the check
/// says so.
fn above_ceiling(
    prepared: &PreparedLauncher,
    repeatable: &[Repeatable<'_>],
    label: &str,
) -> Option<String> {
    let copy_space: usize = repeatable
        .iter()
        .map(|item| copy_space_size(item.loaded))
        .fold(0usize, usize::saturating_add);
    let space = stack_space_size(&prepared.launcher, &prepared.loaded)
        .saturating_mul(copy_space.saturating_add(1));
    (space > COMBINATION_CEILING).then(|| {
        format!(
            "{label}: the selection space has {space} combinations, more than the \
             {COMBINATION_CEILING} this check enumerates, so the cross-combination checks \
             did not run (each axis was validated on its own). Split the launcher or \
             reduce its axes' options"
        )
    })
}

/// Every stack selection the constraints admit, each composed; a
/// selection that composes badly is reported and kept, since the copies
/// are checked over it as well.
fn legal_stacks(
    prepared: &PreparedLauncher,
    ledger: &mut Ledger<'_>,
    label: &str,
    problems: &mut Vec<String>,
) -> Vec<UnitSelection> {
    let mut legal = Vec::new();
    for selection in enumerate_stack(&prepared.launcher, &prepared.loaded) {
        if let Err(e) = check_reach(&prepared.launcher, &prepared.loaded, &selection.entries) {
            problems.push(format!("{label} ({}): {e}", selection.echo()));
            continue;
        }
        let fragments = prepared.loaded.stack_fragments(&selection);
        let in_play = constraints_in_play(&prepared.launcher, &fragments, ConstraintScope::Stack);
        if ledger.judge(&in_play, &selection) {
            continue;
        }
        if let Err(e) = prepared.flat_stack(&selection) {
            problems.push(format!("{label} ({}): {e}", selection.echo()));
        }
        legal.push(selection);
    }
    legal
}

/// The copies the file deploys, with their settings, over every legal
/// stack: what `stack launch --with ...` composes for each selection.
fn check_file_copies_over(
    prepared: &PreparedLauncher,
    stacks: &[UnitSelection],
    label: &str,
) -> Vec<String> {
    let launcher = &prepared.launcher;
    let mut problems = Vec::new();
    for stack in stacks {
        let Ok((_, bare)) = prepared.flat_stack(stack) else {
            // Reported once, by the stack pass.
            continue;
        };
        let mut taken = bare.core_nodes.clone();
        let mut copies = Vec::new();
        for entry in &launcher.option_deployments {
            let loaded = prepared.loaded.option(&entry.axis, &entry.option);
            for instance in &entry.instances {
                let settings = entry.settings_for(instance);
                let echo = format!("{} + file copy `{}`", stack.echo(), instance.instance_id);
                match compose_copy(
                    prepared,
                    stack,
                    &bare,
                    CopyRequest {
                        axis: &entry.axis,
                        loaded,
                        name: &instance.instance_id,
                        with: &settings.with,
                        arguments: &settings.arguments,
                        adjustments: &settings.adjustments,
                    },
                    &taken,
                ) {
                    Ok(copy) => {
                        taken.extend(copy.core_nodes.iter().cloned());
                        copies.push(copy);
                    }
                    Err(e) => problems.push(format!("{label} ({echo}): {e}")),
                }
            }
        }
        if let Err(e) = combine(launcher, &bare, &copies) {
            problems.push(format!("{label} ({}): {e}", stack.echo()));
        }
    }
    problems
}

/// Every copy every legal stack can run: as a launch of one copy, and as a
/// join onto the bare stack. A copy that writes to a stack instance is
/// launch-only, which the join refuses by name at the time. Returns the
/// copy selections some stack admits, by option.
fn check_copies_over(
    prepared: &PreparedLauncher,
    repeatable: &[Repeatable<'_>],
    stacks: &[UnitSelection],
    ledger: &mut Ledger<'_>,
    label: &str,
    problems: &mut Vec<String>,
) -> Vec<(String, UnitSelection)> {
    let launcher = &prepared.launcher;
    let name = Name::try_from(CHECK_COPY.to_owned()).expect("a plain identifier");
    let mut legal_copies: Vec<(String, UnitSelection)> = Vec::new();
    for item in repeatable {
        for copy_selection in enumerate_copy(item.loaded, &item.axis.name) {
            let mut legal_somewhere = false;
            for stack in stacks {
                let full = UnitSelection {
                    entries: stack
                        .launcher_entries(launcher)
                        .into_iter()
                        .chain(copy_selection.entries.iter().cloned())
                        .collect(),
                };
                let fragments = item.loaded.fragments_for(&copy_selection);
                let in_play = constraints_in_play(
                    launcher,
                    &fragments,
                    ConstraintScope::Copy {
                        axis: &item.axis.name,
                    },
                );
                if ledger.judge(&in_play, &full) {
                    continue;
                }
                legal_somewhere = true;
                let (existing, bare) = match prepared.flat_stack(stack) {
                    Ok(result) => result,
                    // Reported once, by the stack pass.
                    Err(_) => continue,
                };
                let with: BTreeMap<String, String> = copy_selection
                    .own_axes(&item.axis.name)
                    .filter_map(|entry| Some((entry.axis.clone(), entry.option.clone()?)))
                    .collect();
                let echo = format!(
                    "{} + copy of {}: {}",
                    stack.echo(),
                    item.option,
                    copy_selection.echo()
                );
                let copy = match compose_copy(
                    prepared,
                    stack,
                    &bare,
                    CopyRequest {
                        axis: &item.axis.name,
                        loaded: item.loaded,
                        name: &name,
                        with: &with,
                        arguments: &BTreeMap::new(),
                        adjustments: &[],
                    },
                    &bare.core_nodes,
                ) {
                    Ok(copy) => copy,
                    Err(e) => {
                        problems.push(format!("{label} ({echo}): {e}"));
                        continue;
                    }
                };
                if let Err(e) = combine(launcher, &bare, std::slice::from_ref(&copy)) {
                    problems.push(format!("{label} ({echo}): {e}"));
                    continue;
                }
                match attach(&existing, &copy) {
                    Ok(_) | Err(CompositionError::JoinChangesExisting { .. }) => {}
                    Err(e) => problems.push(format!("{label} ({echo}, joined): {e}")),
                }
            }
            if legal_somewhere {
                legal_copies.push((item.option.to_owned(), copy_selection));
            }
        }
    }
    legal_copies
}

/// Every state of every axis must survive the constraints somewhere: an
/// option no legal selection can pick is dead weight behind a refusal,
/// and an axis that can never stay off may stay unfilled in name only.
fn dead_states(
    prepared: &PreparedLauncher,
    stacks: &[UnitSelection],
    repeatable: &[Repeatable<'_>],
    legal_copies: &[(String, UnitSelection)],
    label: &str,
) -> Vec<String> {
    let launcher = &prepared.launcher;
    let mut problems = Vec::new();
    for axis in launcher.stack_axes() {
        for state in axis_state_names(axis) {
            let reachable = stacks
                .iter()
                .any(|selection| selection_holds_state(selection, &axis.name, state));
            if !reachable {
                problems.push(dead_state(label, &axis.name, state));
            }
        }
        for (option, loaded) in prepared.loaded.options_of(&axis.name) {
            for own in &loaded.axes {
                for state in axis_state_names(own) {
                    let reachable = stacks.iter().any(|selection| {
                        selection.option_of(&axis.name) == Some(option.as_str())
                            && selection_holds_state(selection, &own.name, state)
                    });
                    if !reachable {
                        problems.push(dead_state(label, &own.name, state));
                    }
                }
            }
        }
    }
    for item in repeatable {
        if !legal_copies.iter().any(|(legal, _)| legal == item.option) {
            problems.push(dead_state(label, &item.axis.name, Some(item.option)));
            continue;
        }
        for own in &item.loaded.axes {
            for state in axis_state_names(own) {
                let reachable = legal_copies.iter().any(|(legal, selection)| {
                    legal == item.option && selection_holds_state(selection, &own.name, state)
                });
                if !reachable {
                    problems.push(dead_state(label, &own.name, state));
                }
            }
        }
    }
    problems
}

/// Every state of one axis by name: each option, then unfilled where the
/// cardinality allows it.
fn axis_state_names(axis: &ComponentAxis) -> Vec<Option<&str>> {
    let mut states: Vec<Option<&str>> = axis
        .options
        .keys()
        .map(|option| Some(option.as_str()))
        .collect();
    if axis.cardinality.allows_empty() {
        states.push(None);
    }
    states
}

fn selection_holds_state(selection: &UnitSelection, axis: &str, state: Option<&str>) -> bool {
    match state {
        Some(option) => selection.options_on(axis).any(|value| value == option),
        None => {
            selection.entries.iter().any(|entry| entry.axis == axis)
                && selection.option_of(axis).is_none()
        }
    }
}

fn dead_state(label: &str, axis: &str, state: Option<&str>) -> String {
    match state {
        Some(option) => format!(
            "{label}: no selection may fill axis `{axis}` with `{option}`: the `constraints` \
             refuse every selection that picks it, so the option can never launch. Loosen a \
             constraint or drop the option"
        ),
        None => format!(
            "{label}: no selection may leave axis `{axis}` unfilled: the `constraints` refuse \
             every selection without it, so its cardinality is a promise nothing keeps. Make \
             the axis required or loosen a constraint"
        ),
    }
}

/// Every state of one axis, in a fixed order: option by option in
/// declaration order, unfilled last where the cardinality allows it.
fn axis_states(axis: &ComponentAxis) -> Vec<SelectionEntry> {
    let mut states: Vec<SelectionEntry> = axis
        .options
        .keys()
        .map(|option| SelectionEntry {
            axis: axis.name.clone(),
            option: Some(option.clone()),
            source: SelectionSource::Explicit,
        })
        .collect();
    if axis.cardinality.allows_empty() {
        states.push(SelectionEntry {
            axis: axis.name.clone(),
            option: None,
            source: SelectionSource::Unfilled,
        });
    }
    states
}

/// Every combination of the states of `axes`, prefixed by `head`.
fn product<'a>(
    head: Vec<SelectionEntry>,
    axes: impl Iterator<Item = &'a ComponentAxis>,
) -> Vec<Vec<SelectionEntry>> {
    axes.fold(vec![head], |selections, axis| {
        let states = axis_states(axis);
        selections
            .iter()
            .flat_map(|selection| {
                states.iter().map(|state| {
                    selection
                        .iter()
                        .cloned()
                        .chain(std::iter::once(state.clone()))
                        .collect()
                })
            })
            .collect()
    })
}

/// Every selection of the stack by shape: the launcher's own axes, then
/// the axes of whichever options each combination selects.
fn enumerate_stack(launcher: &PeppyLauncher, loaded: &LoadedComposition) -> Vec<UnitSelection> {
    product(Vec::new(), launcher.stack_axes())
        .into_iter()
        .flat_map(|entries| {
            let nested: Vec<&ComponentAxis> = entries
                .iter()
                .filter_map(|entry| {
                    let option = entry.option.as_ref()?;
                    Some(loaded.option(&entry.axis, option))
                })
                .flat_map(|loaded_option| loaded_option.axes.iter())
                .collect();
            product(entries, nested.into_iter())
        })
        .map(|entries| UnitSelection { entries })
        .collect()
}

/// Every selection of one copied option's own axes.
fn enumerate_copy(loaded: &LoadedOption, parent_axis: &str) -> Vec<UnitSelection> {
    let head = vec![SelectionEntry {
        axis: parent_axis.to_owned(),
        option: Some(loaded.name.clone()),
        source: SelectionSource::Explicit,
    }];
    product(head, loaded.axes.iter())
        .into_iter()
        .map(|entries| UnitSelection { entries })
        .collect()
}

fn axis_state_count(axis: &ComponentAxis) -> usize {
    axis.options
        .len()
        .saturating_add(usize::from(axis.cardinality.allows_empty()))
}

/// The size of [`enumerate_copy`]'s result, computed before anything is
/// allocated so an oversized family is refused cheaply.
fn copy_space_size(loaded: &LoadedOption) -> usize {
    loaded
        .axes
        .iter()
        .map(axis_state_count)
        .fold(1usize, usize::saturating_mul)
}

/// The size of [`enumerate_stack`]'s result.
fn stack_space_size(launcher: &PeppyLauncher, loaded: &LoadedComposition) -> usize {
    launcher
        .stack_axes()
        .map(|axis| {
            axis.options
                .keys()
                .map(|option| copy_space_size(loaded.option(&axis.name, option)))
                .fold(0usize, usize::saturating_add)
                .saturating_add(usize::from(axis.cardinality.allows_empty()))
        })
        .fold(1usize, usize::saturating_mul)
}

/// Which constraints refused something, and which never do. A constraint is
/// identified by the document declaring it and its position there; a
/// fragment file shared by several options is one document.
struct Ledger<'a> {
    /// Every constraint of the launcher and of every fragment, each once.
    constraints: Vec<ConstraintInPlay<'a>>,
    /// The ledger position of each constraint's key.
    positions: HashMap<(Option<usize>, usize), usize>,
    refused: Vec<usize>,
    /// For each constraint, the first constraint seen refusing a selection
    /// it also speaks about: itself when it is the first violation, an
    /// earlier one when something in front of it refuses that selection
    /// first. A constraint that refuses nothing while this names an earlier
    /// constraint is dead because that constraint shadows it; None means no
    /// selection ever spoke about it at all.
    first_refuser: Vec<Option<usize>>,
}

impl<'a> Ledger<'a> {
    fn new(prepared: &'a PreparedLauncher) -> Self {
        let mut constraints: Vec<ConstraintInPlay<'a>> = Vec::new();
        let mut positions: HashMap<(Option<usize>, usize), usize> = HashMap::new();
        for (position, constraint) in prepared.launcher.constraints.iter().enumerate() {
            positions.insert((None, position), constraints.len());
            constraints.push(ConstraintInPlay {
                constraint,
                origin: LAUNCHER,
                key: (None, position),
            });
        }
        for axis in &prepared.launcher.components {
            for (_, loaded) in prepared.loaded.options_of(&axis.name) {
                for fragment in loaded.all_fragments() {
                    for (position, constraint) in fragment.body.constraints.iter().enumerate() {
                        let key = (Some(fragment.id), position);
                        positions.entry(key).or_insert_with(|| {
                            constraints.push(ConstraintInPlay {
                                constraint,
                                origin: &fragment.origin,
                                key,
                            });
                            constraints.len() - 1
                        });
                    }
                }
            }
        }
        let count = constraints.len();
        Self {
            constraints,
            positions,
            refused: vec![0; count],
            first_refuser: vec![None; count],
        }
    }

    /// Records who refuses `selection`, if anyone. Returns whether it was
    /// refused.
    fn judge(&mut self, in_play: &[ConstraintInPlay<'_>], selection: &UnitSelection) -> bool {
        let violated: Vec<usize> = in_play
            .iter()
            .map(|constraint| self.positions[&constraint.key])
            .filter(|index| !constraint_satisfied(selection, self.constraints[*index].constraint))
            .collect();
        let Some(&first) = violated.first() else {
            return false;
        };
        self.refused[first] += 1;
        for index in violated {
            self.first_refuser[index].get_or_insert(first);
        }
        true
    }

    /// A constraint that refuses nothing is dead: every selection it speaks
    /// about already satisfies it, or an earlier constraint refuses those
    /// selections first. Either way its reason is never read, and a dead
    /// rule reads as protection it does not give.
    fn dead_constraints(&self, label: &str) -> Vec<String> {
        self.refused
            .iter()
            .enumerate()
            .filter(|(_, refused)| **refused == 0)
            .map(|(index, _)| {
                let why = match self.first_refuser[index].filter(|first| *first != index) {
                    Some(first) => format!(
                        "the selections it would refuse are already refused by earlier \
                         constraints ({} refuses them first)",
                        render_constraint(self.constraints[first])
                    ),
                    None => String::from(
                        "it is already guaranteed by the axes or by an earlier constraint",
                    ),
                };
                format!(
                    "{label}: constraint `{}` refuses no selection: {why}. Tighten it or drop it",
                    render_constraint(self.constraints[index]),
                )
            })
            .collect()
    }
}
