//! The audit trail of one composition: what was selected, which adjustments
//! applied (with the values they replaced), and which were skipped (and
//! why). `peppy stack resolve` prints it; a launch holds it for the echo.

use super::super::types::LinkValue;
use super::copy::CopyRecord;
use super::select::UnitSelection;
use config::AnyType;

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
}

impl AppliedChange {
    /// The written field, `arguments.<key>` or `links.<slot>`.
    pub fn field(&self) -> String {
        match self {
            AppliedChange::Argument { key, .. } => format!("arguments.{key}"),
            AppliedChange::LinkSet { slot, .. }
            | AppliedChange::LinkAdded { slot, .. }
            | AppliedChange::LinkRemoved { slot, .. } => format!("links.{slot}"),
        }
    }

    /// The value the change leaves in its field, as a conflict names it.
    pub(super) fn written(&self) -> String {
        match self {
            AppliedChange::Argument { new, .. } => render(new),
            AppliedChange::LinkSet { new, .. } => render(new),
            AppliedChange::LinkAdded { target, .. } => format!("+ {target}"),
            AppliedChange::LinkRemoved { .. } => String::from("(absent)"),
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
        }
    }
}

/// What one applied adjustment did: the target and field it touched, the
/// before and after, and the fragment (or base) it came from.
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
    /// the recorder's attach simply does not run when no recorder was
    /// selected.
    TargetAbsent,
    /// The `when` guard named an axis whose selected option is not the one
    /// the guard requires.
    GuardNotMet(String),
    /// The copy axis carried here reaches the adjustment: its guard names
    /// that axis, or that axis's options alone define its target. The
    /// adjustment belongs to each copy, whose own report carries it.
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
    pub applied: Vec<AppliedAdjustment>,
    pub skipped: Vec<SkippedAdjustment>,
}

impl CompositionReport {
    /// What was selected, one line for the stack (when the launcher has
    /// axes of its own) and one per copy: what a launch echoes before
    /// anything runs.
    pub fn selection_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let echo = self.selection.echo();
        if !echo.is_empty() {
            lines.push(format!("components: {echo}"));
        }
        for copy in &self.copies {
            lines.push(format!("copy {}: {}", copy.name, copy.selection.echo()));
        }
        lines
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
