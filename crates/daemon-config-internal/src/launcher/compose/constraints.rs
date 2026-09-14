//! Constraints and guards read a selection the same way: an `axis: options`
//! map holds when every named axis is filled with one of its options. This
//! module evaluates them and renders the refusals.

use super::super::composition::{SelectionCondition, SelectionConstraint};
use super::super::types::PeppyLauncher;
use super::error::CompositionError;
use super::load::LoadedFragment;
use super::select::UnitSelection;

/// How the launcher names its own constraints in a refusal.
pub(super) const LAUNCHER: &str = "the launcher";

/// A constraint in play for one unit, with the document that declares it.
#[derive(Clone, Copy)]
pub(super) struct ConstraintInPlay<'a> {
    pub constraint: &'a SelectionConstraint,
    pub origin: &'a str,
    /// The document declaring the constraint and its position there: the
    /// launcher (`None`) or a loaded fragment.
    pub key: (Option<usize>, usize),
}

/// The unit of a launch a constraint set is assembled for.
#[derive(Clone, Copy)]
pub(super) enum ConstraintScope<'a> {
    /// The stack: the launcher's constraints that name no copy axis.
    Stack,
    /// One copy of the named axis: the launcher's constraints that name
    /// that axis.
    Copy { axis: &'a str },
}

/// The constraints `scope` answers to: the launcher's, scoped to it, then
/// those of every fragment in play, in declaration order.
pub(super) fn constraints_in_play<'a>(
    launcher: &'a PeppyLauncher,
    fragments: &[&'a LoadedFragment],
    scope: ConstraintScope<'_>,
) -> Vec<ConstraintInPlay<'a>> {
    let repeatable: Vec<&str> = launcher
        .repeatable_axes()
        .map(|axis| axis.name.as_str())
        .collect();
    launcher
        .constraints
        .iter()
        .enumerate()
        .filter(|(_, constraint)| match scope {
            ConstraintScope::Stack => !repeatable
                .iter()
                .any(|axis| constraint_names_axis(constraint, axis)),
            ConstraintScope::Copy { axis } => constraint_names_axis(constraint, axis),
        })
        .map(|(position, constraint)| ConstraintInPlay {
            constraint,
            origin: LAUNCHER,
            key: (None, position),
        })
        .chain(fragments.iter().flat_map(|fragment| {
            fragment
                .body
                .constraints
                .iter()
                .enumerate()
                .map(|(position, constraint)| ConstraintInPlay {
                    constraint,
                    origin: &fragment.origin,
                    key: (Some(fragment.id), position),
                })
        }))
        .collect()
}

/// Every guard entry must match the selection: naming several axes is an
/// AND, which is how a base writes "only when the headset leads the real
/// robot".
pub(super) fn guard_holds(selection: &UnitSelection, when: &SelectionCondition) -> bool {
    when.iter().all(|(axis, options)| {
        selection
            .options_on(axis)
            .any(|option| options.contains(option))
    })
}

/// Whether a guard names `axis` at all.
pub(super) fn names_axis(condition: Option<&SelectionCondition>, axis: &str) -> bool {
    condition.is_some_and(|map| map.contains_key(axis))
}

/// Whether a constraint's guard, `requires` or `forbids` names `axis`.
pub(super) fn constraint_names_axis(constraint: &SelectionConstraint, axis: &str) -> bool {
    names_axis(constraint.when.as_ref(), axis)
        || constraint
            .requires
            .iter()
            .chain(&constraint.forbids)
            .any(|entry| entry.contains_key(axis))
}

pub(super) fn render_guard(when: &SelectionCondition) -> String {
    when.iter()
        .map(|(axis, option)| format!("{axis}={option}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether this selection satisfies one constraint: either the constraint's
/// guard does not speak about it, or it matches no `forbids` entry and (when
/// any are declared) at least one `requires` alternative holds wholly. Both
/// lists use the same matching a guard does, so an unfilled optional axis
/// satisfies no pair on either side.
pub(super) fn constraint_satisfied(
    selection: &UnitSelection,
    constraint: &SelectionConstraint,
) -> bool {
    if let Some(when) = &constraint.when
        && !guard_holds(selection, when)
    {
        return true;
    }
    if constraint
        .forbids
        .iter()
        .any(|entry| guard_holds(selection, entry))
    {
        return false;
    }
    constraint.requires.is_empty()
        || constraint
            .requires
            .iter()
            .any(|alternative| guard_holds(selection, alternative))
}

/// The first declared constraint this selection violates. Declaration
/// order, so overlapping constraints refuse deterministically and the
/// author controls which reason the operator reads first.
fn first_violated<'a>(
    constraints: &[ConstraintInPlay<'a>],
    selection: &UnitSelection,
) -> Option<ConstraintInPlay<'a>> {
    constraints
        .iter()
        .copied()
        .find(|in_play| !constraint_satisfied(selection, in_play.constraint))
}

/// Refuses `selection` when a constraint in play refuses it.
pub(super) fn check(
    constraints: &[ConstraintInPlay<'_>],
    selection: &UnitSelection,
) -> Result<(), CompositionError> {
    match first_violated(constraints, selection) {
        Some(violated) => Err(constraint_violation(violated, selection)),
        None => Ok(()),
    }
}

/// The refusal for a selection that violates a constraint, carrying the full
/// resolved selection (whose `(from file)` markers show the axes the operator
/// did not name), what was required or forbidden, and the author's reason.
/// A matched `forbids` entry is the sharper claim, so it is the one
/// reported when a selection fails on both counts.
fn constraint_violation(
    in_play: ConstraintInPlay<'_>,
    selection: &UnitSelection,
) -> CompositionError {
    let constraint = in_play.constraint;
    if let Some(matched) = constraint
        .forbids
        .iter()
        .find(|entry| guard_holds(selection, entry))
    {
        return CompositionError::ConstraintForbidden {
            condition: render_condition(in_play),
            matched: format!("`{}`", render_guard(matched)),
            selection: selection.echo(),
            reason: constraint.reason.clone(),
        };
    }
    CompositionError::ConstraintUnsatisfied {
        condition: render_condition(in_play),
        alternatives: render_alternatives(&constraint.requires),
        selection: selection.echo(),
        reason: constraint.reason.clone(),
    }
}

fn render_condition(in_play: ConstraintInPlay<'_>) -> String {
    match &in_play.constraint.when {
        Some(when) => format!("{} selecting {}", in_play.origin, render_guard(when)),
        None => in_play.origin.to_owned(),
    }
}

fn render_alternatives(alternatives: &[SelectionCondition]) -> String {
    let rendered: Vec<String> = alternatives
        .iter()
        .map(|alternative| format!("`{}`", render_guard(alternative)))
        .collect();
    match rendered.as_slice() {
        [single] => single.clone(),
        many => format!("one of {}", many.join(", ")),
    }
}

/// One constraint in a line, for the check-time problems that name a whole
/// rule: "the launcher selecting cameras=cameras requires `robot=real`",
/// "the launcher forbids `robot=mujoco recorder=on`".
pub(super) fn render_constraint(in_play: ConstraintInPlay<'_>) -> String {
    let constraint = in_play.constraint;
    let mut clauses = Vec::with_capacity(2);
    if !constraint.requires.is_empty() {
        clauses.push(format!(
            "requires {}",
            render_alternatives(&constraint.requires)
        ));
    }
    if !constraint.forbids.is_empty() {
        let entries: Vec<String> = constraint
            .forbids
            .iter()
            .map(|entry| format!("`{}`", render_guard(entry)))
            .collect();
        clauses.push(format!("forbids {}", entries.join(", ")));
    }
    format!("{} {}", render_condition(in_play), clauses.join(" and "))
}
