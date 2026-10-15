//! The audit trail of one composition: what was selected, which adjustments
//! applied (with the values they replaced), and which were skipped (and
//! why). `peppy stack resolve` prints it; a launch holds it for the echo.

use super::super::composition::copy_adjustments_origin;
use super::super::types::LinkValue;
use super::copy::CopyRecord;
use super::select::{PartMachines, UnitSelection};
use config::AnyType;
use config::node::RestartDeclaration;
use config::runtime::{CopyName, Name};
use std::collections::BTreeMap;

/// One field an adjustment wrote, with the value it replaced.
#[derive(Debug, Clone, PartialEq)]
pub enum AppliedChange {
    Argument {
        key: String,
        old: Option<AnyType>,
        new: AnyType,
    },
    LinkSet {
        slot: String,
        old: Option<LinkValue>,
        new: LinkValue,
    },
    LinkAdded {
        slot: String,
        target: String,
    },
    LinkRemoved {
        slot: String,
        old: LinkValue,
    },
    Clock {
        old: Option<Name>,
        new: Name,
    },
    /// The scope of one daemon target the instance serves.
    DaemonScope {
        daemon_target: String,
        old: Option<serde_json::Value>,
        new: serde_json::Value,
    },
    /// The restart policy of the instance.
    Restart {
        old: Option<RestartDeclaration>,
        new: RestartDeclaration,
    },
}

impl AppliedChange {
    /// The written field, `arguments.<key>`, `links.<slot>`,
    /// `framework.clock`, `framework.restart` or `daemon_scopes.<target>`.
    pub fn field(&self) -> String {
        match self {
            AppliedChange::Argument { key, .. } => format!("arguments.{key}"),
            AppliedChange::LinkSet { slot, .. }
            | AppliedChange::LinkAdded { slot, .. }
            | AppliedChange::LinkRemoved { slot, .. } => format!("links.{slot}"),
            AppliedChange::Clock { .. } => String::from("framework.clock"),
            AppliedChange::DaemonScope { daemon_target, .. } => {
                format!("daemon_scopes.{daemon_target}")
            }
            AppliedChange::Restart { .. } => String::from("framework.restart"),
        }
    }

    /// The value the change leaves in its field, as a conflict names it.
    pub(super) fn written(&self) -> String {
        match self {
            AppliedChange::Argument { new, .. } => render(new),
            AppliedChange::LinkSet { new, .. } => render(new),
            AppliedChange::LinkAdded { target, .. } => format!("+ {target}"),
            AppliedChange::LinkRemoved { .. } => String::from("(absent)"),
            AppliedChange::Clock { new, .. } => new.to_string(),
            AppliedChange::DaemonScope { new, .. } => render(new),
            AppliedChange::Restart { new, .. } => new.launcher_spelling(),
        }
    }

    fn render(&self) -> String {
        match self {
            AppliedChange::Argument { old, new, .. } => {
                format!("{} -> {}", render_option(old.as_ref()), render(new))
            }
            AppliedChange::LinkSet { old, new, .. } => {
                format!("{} -> {}", render_option(old.as_ref()), render(new))
            }
            AppliedChange::LinkAdded { target, .. } => format!("+ {target}"),
            AppliedChange::LinkRemoved { old, .. } => format!("removed (was {})", render(old)),
            AppliedChange::Clock { old, new } => {
                transition(old.as_ref().map(Name::to_string), new.to_string())
            }
            AppliedChange::Restart { old, new } => transition(
                old.as_ref().map(RestartDeclaration::launcher_spelling),
                new.launcher_spelling(),
            ),
            AppliedChange::DaemonScope { old, new, .. } => {
                format!("{} -> {}", render_option(old.as_ref()), render(new))
            }
        }
    }
}

/// `old -> new` for a framework knob, `(absent)` standing for a knob the
/// entry did not write.
pub(super) fn transition(old: Option<String>, new: String) -> String {
    format!(
        "{} -> {new}",
        old.unwrap_or_else(|| String::from("(absent)"))
    )
}

/// What one applied adjustment did: the target and field it touched, the
/// before and after, and the launcher entry it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedAdjustment {
    pub target: String,
    pub change: AppliedChange,
    pub origin: String,
}

/// Why an adjustment did not run.
#[derive(Debug, Clone)]
pub enum SkipReason {
    /// The target the selection resolved to does not define this instance:
    /// a top-level write to `sim_inst` does not run when the robot is real.
    TargetAbsent,
    /// The `when` guard named an axis whose selected option is not the one
    /// the guard requires.
    GuardNotMet(String),
    /// The adjustment runs in each copy of the axis carried here, whose own
    /// report carries it: its guard names the axis, or its target is one
    /// that only the axis's options define. One line per such axis.
    RunsInCopies(String),
}

impl SkipReason {
    fn render(&self) -> String {
        match self {
            SkipReason::TargetAbsent => "target is not in this selection".to_owned(),
            SkipReason::GuardNotMet(guard) => format!("guard {guard} is not met"),
            SkipReason::RunsInCopies(axis) => {
                format!("axis {axis} runs as copies; the adjustment runs in each copy")
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct SkippedAdjustment {
    pub target: String,
    pub reason: SkipReason,
    pub origin: String,
}

#[derive(Debug, Clone, Default)]
pub struct CompositionReport {
    /// The stack's resolution: the launcher's own axes and those of the
    /// options they selected.
    pub selection: UnitSelection,
    pub copies: Vec<CopyRecord>,
    /// The machine of each placed part of each copy, by copy and own axis:
    /// the parts the words place, which the launch and the join place there.
    pub part_machines: BTreeMap<CopyName, PartMachines>,
    pub applied: Vec<AppliedAdjustment>,
    pub skipped: Vec<SkippedAdjustment>,
}

impl CompositionReport {
    /// What was selected, one line for the stack (when the launcher has
    /// axes of its own) and one per copy, each placed part with its machine:
    /// what a launch echoes before anything runs.
    pub fn selection_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let echo = self.selection.echo();
        if !echo.is_empty() {
            lines.push(format!("components: {echo}"));
        }
        lines.extend(self.copies.iter().map(|copy| {
            format!(
                "copy {}: {}",
                copy.name,
                copy.selection.placed_echo(self.parts_of(&copy.name))
            )
        }));
        lines
    }

    /// The machine of each placed part of the copy `name`, by own axis.
    pub fn parts_of(&self, name: &CopyName) -> &PartMachines {
        static NONE: PartMachines = PartMachines::new();
        self.part_machines.get(name).unwrap_or(&NONE)
    }

    /// Every applied adjustment with the value it replaced, then every
    /// skipped one with its reason.
    fn adjustment_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if !self.applied.is_empty() {
            lines.push(String::from("adjustments applied:"));
            for entry in &self.applied {
                lines.push(format!(
                    "  {}.{}: {}  ({})",
                    entry.target,
                    entry.change.field(),
                    entry.change.render(),
                    entry.origin
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

    /// The report body `peppy stack resolve` writes to stderr.
    pub fn render_lines(&self) -> Vec<String> {
        let mut lines = self.selection_lines();
        lines.extend(self.adjustment_lines());
        lines
    }

    /// Folds one copy of a launch in: its record, and the adjustments it
    /// applied and skipped, attributed.
    pub(super) fn add_copy(
        &mut self,
        copy: CopyRecord,
        parts: PartMachines,
        applied: Vec<AppliedAdjustment>,
        skipped: Vec<SkippedAdjustment>,
    ) {
        if !parts.is_empty() {
            self.part_machines.insert(copy.name.clone(), parts);
        }
        self.applied.extend(applied.into_iter().map(|mut entry| {
            entry.origin = attributed(&copy, &entry.origin, &entry.target);
            entry
        }));
        self.skipped.extend(skipped.into_iter().map(|mut entry| {
            entry.origin = attributed(&copy, &entry.origin, &entry.target);
            entry
        }));
        self.copies.push(copy);
    }

    /// Folds one join's whole report in: its copy and the entries that copy
    /// already carries, attributed when it was composed.
    pub(super) fn absorb(&mut self, joined: CompositionReport) {
        self.copies.extend(joined.copies);
        self.part_machines.extend(joined.part_machines);
        self.applied.extend(joined.applied);
        self.skipped.extend(joined.skipped);
    }
}

/// Where one line of a copy's report came from: the origin alone when the
/// copy runs the instance or the line is the copy's own adjustment, the
/// copy behind it otherwise, so a line about an instance the copy does not
/// run says which copy asked for it.
fn attributed(copy: &CopyRecord, origin: &str, target: &str) -> String {
    if copy.owns_instance(target) || origin == copy_adjustments_origin(&copy.name) {
        origin.to_owned()
    } else {
        format!("{origin}, copy `{}`", copy.name)
    }
}

pub(super) fn render<T: serde::Serialize>(value: &T) -> String {
    serde_json5::to_string(value).unwrap_or_else(|_| String::from("(unrenderable)"))
}

/// Renders an optional value: the value, or `(absent)`.
pub(super) fn render_option<T: serde::Serialize>(value: Option<&T>) -> String {
    match value {
        Some(value) => render(value),
        None => String::from("(absent)"),
    }
}
