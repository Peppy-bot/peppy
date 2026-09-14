//! A launcher held ready to compose: its fragments read once, and the
//! launch, join and removal it answers.

use super::super::composition::Adjustment;
use super::super::types::PeppyLauncher;
use super::constraints::{self, ConstraintScope};
use super::copy::{
    self, ComposedCopy, CopyRecord, CopyRequest, argument_overrides, combine, compose_copy,
    flat_document, validate_flat,
};
use super::error::CompositionError;
use super::expand::{Expanded, OriginatedDeployment, Unit, expand_unit};
use super::load::{LoadedComposition, launcher_file_label, load_composition};
use super::report::{CompositionReport, SkipReason, SkippedAdjustment};
use super::select::{self, CopyOrigin, LaunchWords, UnitSelection};
use config::runtime::Name;
use core_node_api::encoding::ArgumentOverride;
use std::collections::HashSet;
use std::path::Path;

/// A launcher's complete fragment contents, captured when it is launched.
#[derive(Debug, Clone)]
pub struct PreparedLauncher {
    pub(super) launcher: PeppyLauncher,
    pub(super) loaded: LoadedComposition,
    pub(super) label: String,
}

/// A composed launch: the flat document, the stack's selection every later
/// join composes against, and the report, which carries the copies the
/// launcher deployed.
#[derive(Debug)]
pub struct ComposedLaunch {
    pub launcher: PeppyLauncher,
    pub selection: UnitSelection,
    pub report: CompositionReport,
}

impl ComposedLaunch {
    /// The copies the launcher deployed, in the order it lists them.
    pub fn copies(&self) -> &[CopyRecord] {
        &self.report.copies
    }
}

/// A composed join: the running stack with one more copy.
#[derive(Debug)]
pub struct ComposedJoin {
    pub launcher: PeppyLauncher,
    pub copy: CopyRecord,
    pub report: CompositionReport,
}

/// What `peppy stack join` asks for: the option to copy, the copy's name,
/// the `--with` words over the option's own axes, and its
/// `--set-arguments` overrides.
#[derive(Debug, Clone, Copy)]
pub struct JoinRequest<'a> {
    pub option: &'a str,
    pub name: &'a Name,
    pub words: &'a [String],
    pub arguments: &'a [ArgumentOverride],
}

/// The stack as it runs: its selection and its flat document.
#[derive(Debug, Clone, Copy)]
pub struct RunningStack<'a> {
    pub selection: &'a UnitSelection,
    pub launcher: &'a PeppyLauncher,
}

impl PreparedLauncher {
    pub fn load(launcher: &PeppyLauncher, file: &Path) -> Result<Self, CompositionError> {
        Ok(Self {
            loaded: load_composition(launcher, file)?,
            launcher: launcher.clone(),
            label: launcher_file_label(file),
        })
    }

    /// Composes a launch: the stack under `words`, then every copy the
    /// launcher's `deployments` list.
    pub fn launch(&self, words: &[String]) -> Result<ComposedLaunch, CompositionError> {
        if self.launcher.components.is_empty() {
            if !words.is_empty() {
                return Err(CompositionError::WithOnFlatLauncher);
            }
            return Ok(ComposedLaunch {
                launcher: self.launcher.clone(),
                selection: UnitSelection::default(),
                report: CompositionReport::default(),
            });
        }
        let LaunchWords { scoped, stack } = select::split_scoped_words(&self.launcher, words)?;
        let selection = select::resolve_stack(&self.launcher, &self.loaded, &stack)?;
        let bare = self.compose_stack(&selection)?;
        let mut taken = bare.core_nodes.clone();
        let mut copies: Vec<ComposedCopy> = Vec::new();
        for entry in &self.launcher.option_deployments {
            let loaded = self.loaded.option(&entry.axis, &entry.option);
            for instance in &entry.instances {
                let settings = entry.settings_for(instance);
                let mut with = settings.with;
                if let Some(rests) = scoped.get(instance.instance_id.as_str()) {
                    // A refusal names the word as typed, copy and all.
                    let as_typed = |rest: &str| format!("{}.{rest}", instance.instance_id);
                    let chosen =
                        select::copy_words(loaded, rests).map_err(|error| match error {
                            CompositionError::UnknownCopySelection { word, option, menu } => {
                                CompositionError::UnknownCopySelection {
                                    word: as_typed(&word),
                                    option,
                                    menu,
                                }
                            }
                            CompositionError::AmbiguousSelection { word, axes } => {
                                CompositionError::AmbiguousSelection {
                                    word: as_typed(&word),
                                    axes,
                                }
                            }
                            other => other,
                        })?;
                    with.extend(chosen);
                }
                let copy = compose_copy(
                    self,
                    &selection,
                    &bare,
                    CopyRequest {
                        axis: &entry.axis,
                        loaded,
                        name: &instance.instance_id,
                        with: &with,
                        arguments: &settings.arguments,
                        adjustments: &settings.adjustments,
                        origin: CopyOrigin::File,
                    },
                    &taken,
                )?;
                taken.extend(copy.core_nodes.iter().cloned());
                copies.push(copy);
            }
        }
        let launcher = combine(&self.launcher, &bare, &copies)?;
        let mut report = CompositionReport {
            selection: selection.clone(),
            copies: Vec::new(),
            applied: bare.applied,
            skipped: bare.skipped,
        };
        for copy in copies {
            let record = copy.record();
            // A line about a stack instance names the copy behind it.
            let owned = |target: &str| record.instance_ids.iter().any(|id| id.as_str() == target);
            let attributed = |origin: &str, target: &str| {
                if owned(target) {
                    origin.to_owned()
                } else {
                    format!("{origin}, copy `{}`", record.name)
                }
            };
            report
                .applied
                .extend(copy.applied.into_iter().map(|mut entry| {
                    entry.origin = attributed(&entry.origin, &entry.target);
                    entry
                }));
            report
                .skipped
                .extend(copy.skipped.into_iter().map(|mut entry| {
                    entry.origin = attributed(&entry.origin, &entry.target);
                    entry
                }));
            report.copies.push(record);
        }
        Ok(ComposedLaunch {
            launcher,
            selection,
            report,
        })
    }

    /// Composes one more copy over the running stack.
    pub fn join(
        &self,
        request: JoinRequest<'_>,
        stack: RunningStack<'_>,
    ) -> Result<ComposedJoin, CompositionError> {
        let mut repeatable = self.launcher.repeatable_axes().peekable();
        if repeatable.peek().is_none() {
            return Err(CompositionError::NoRepeatableAxis);
        }
        let Some(axis) = repeatable.find(|axis| axis.options.contains_key(request.option)) else {
            return Err(CompositionError::JoinUnknownOption {
                option: request.option.to_owned(),
                menu: select::axes_menu(self.launcher.repeatable_axes()),
            });
        };
        let loaded = self.loaded.option(&axis.name, request.option);
        let with = select::copy_words(loaded, request.words)?;
        let arguments = argument_overrides(request.arguments)?;
        let bare = self.compose_stack(stack.selection)?;
        let copy = compose_copy(
            self,
            stack.selection,
            &bare,
            CopyRequest {
                axis: &axis.name,
                loaded,
                name: request.name,
                with: &with,
                arguments: &arguments,
                adjustments: &[],
                origin: CopyOrigin::Join,
            },
            &stack.launcher.core_nodes,
        )?;
        let launcher = copy::attach(stack.launcher, &copy)?;
        let record = copy.record();
        let report = CompositionReport {
            selection: stack.selection.clone(),
            copies: vec![record.clone()],
            applied: copy::against_running(stack.launcher, copy.applied),
            skipped: copy.skipped,
        };
        Ok(ComposedJoin {
            launcher,
            copy: record,
            report,
        })
    }

    /// The running stack without one of its copies.
    pub fn remove(
        &self,
        existing: &PeppyLauncher,
        copy: &CopyRecord,
    ) -> Result<PeppyLauncher, CompositionError> {
        let known = self
            .launcher
            .repeatable_axes()
            .any(|axis| axis.name == copy.axis && axis.options.contains_key(&copy.option));
        if !known {
            return Err(CompositionError::CopyOfAnotherLauncher {
                copy: copy.name.to_string(),
                axis: copy.axis.clone(),
                option: copy.option.clone(),
            });
        }
        copy::detach(existing, copy)
    }

    /// The stack alone under `selection`: the launcher's own deployments,
    /// the selected options' fragments and those of their own selected
    /// options, with the constraints in play checked first.
    fn compose_stack(&self, selection: &UnitSelection) -> Result<Expanded, CompositionError> {
        let fragments = self.loaded.stack_fragments(selection);
        let in_play =
            constraints::constraints_in_play(&self.launcher, &fragments, ConstraintScope::Stack);
        constraints::check(&in_play, selection)?;
        let copy_axes: Vec<CopyAxis<'_>> = self
            .launcher
            .repeatable_axes()
            .map(|axis| CopyAxis {
                name: axis.name.as_str(),
                defines: self
                    .loaded
                    .options_of(&axis.name)
                    .flat_map(|(_, option)| option.definable_ids())
                    .collect(),
            })
            .collect();
        // The ids a stack option or the base can define.
        let stack_ids: HashSet<&str> = self
            .launcher
            .deployments
            .iter()
            .flat_map(|deployment| &deployment.instances)
            .map(|instance| instance.instance_id.as_str())
            .chain(
                self.launcher
                    .stack_axes()
                    .flat_map(|axis| self.loaded.options_of(&axis.name))
                    .flat_map(|(_, option)| option.definable_ids()),
            )
            .collect();
        let base_adjustments = self
            .launcher
            .adjustments
            .iter()
            .filter(|adjustment| copy_axis_of(adjustment, &stack_ids, &copy_axes).is_none())
            .collect();
        let base_origin = format!("{} (base)", self.label);
        let unit = Unit {
            base: self
                .launcher
                .deployments
                .iter()
                .map(|deployment| OriginatedDeployment {
                    deployment,
                    origin: base_origin.clone(),
                })
                .collect(),
            fragments,
            base_adjustments,
            base_origin: base_origin.clone(),
            copy_adjustments: Vec::new(),
            selection: selection.clone(),
        };
        let mut expanded = expand_unit(&unit, &self.launcher.core_nodes)?;
        let in_copies = self.launcher.adjustments.iter().filter_map(|adjustment| {
            let axis = copy_axis_of(adjustment, &stack_ids, &copy_axes)?;
            Some(SkippedAdjustment {
                target: adjustment.target.to_string(),
                reason: SkipReason::RunsInCopies(axis.to_owned()),
                origin: base_origin.clone(),
            })
        });
        expanded.skipped.extend(in_copies);
        Ok(expanded)
    }

    /// The stack alone as a validated flat document.
    pub(super) fn flat_stack(
        &self,
        selection: &UnitSelection,
    ) -> Result<(PeppyLauncher, Expanded), CompositionError> {
        let bare = self.compose_stack(selection)?;
        let flat = validate_flat(&flat_document(
            &self.launcher,
            bare.deployments.clone(),
            bare.core_nodes.clone(),
        ))?;
        Ok((flat, bare))
    }
}

/// One `zero_or_more` axis of a launcher, with every instance id its
/// options can define.
struct CopyAxis<'a> {
    name: &'a str,
    defines: HashSet<&'a str>,
}

/// The copy axis that reaches one launcher adjustment: the axis its guard
/// names, or the axis whose options alone define its target. Each copy of
/// that axis runs the adjustment, so the stack does not.
fn copy_axis_of<'a>(
    adjustment: &Adjustment,
    stack_ids: &HashSet<&str>,
    copy_axes: &[CopyAxis<'a>],
) -> Option<&'a str> {
    let guarded = copy_axes
        .iter()
        .find(|axis| constraints::names_axis(adjustment.when.as_ref(), axis.name));
    if let Some(axis) = guarded {
        return Some(axis.name);
    }
    let target = adjustment.target.as_str();
    if stack_ids.contains(target) {
        return None;
    }
    copy_axes
        .iter()
        .find(|axis| axis.defines.contains(target))
        .map(|axis| axis.name)
}
