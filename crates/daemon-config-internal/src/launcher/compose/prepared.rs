//! A launcher held ready to compose: its fragments read once, and the
//! launch, join and removal it answers.

use super::super::composition::{
    Adjustment, ArgumentOverrides, ComponentCardinality, CopySettings, OptionDeployment,
};
use super::super::types::{LauncherFramework, PeppyLauncher};
use super::constraints::{self, ConstraintScope};
use super::copy::{
    self, ComposedCopy, CopyRecord, CopyRequest, argument_overrides, combine, compose_copy,
    flat_document, validate_flat,
};
use super::error::CompositionError;
use super::expand::{Expanded, OriginatedDeployment, Unit, expand_unit, merge_clocks};
use super::load::{LoadedComposition, LoadedOption, launcher_file_label, load_composition};
use super::report::{CompositionReport, SkipReason, SkippedAdjustment};
use super::select::{self, CopyOrigin, LaunchWords, UnitSelection};
use config::runtime::Name;
use core_node_api::encoding::{ArgumentOverride, LaunchJoin};
use std::collections::{BTreeMap, HashSet};
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
    /// launcher's `deployments` list, then every copy `joins` names, each
    /// joined onto the plan the ones before it left.
    pub fn launch(
        &self,
        words: &[String],
        joins: &[LaunchJoin],
    ) -> Result<ComposedLaunch, CompositionError> {
        if self.launcher.components.is_empty() {
            if !words.is_empty() {
                return Err(CompositionError::WithOnFlatLauncher);
            }
            if !joins.is_empty() {
                return Err(CompositionError::NoRepeatableAxis);
            }
            return Ok(ComposedLaunch {
                launcher: self.launcher.clone(),
                selection: UnitSelection::default(),
                report: CompositionReport::default(),
            });
        }
        let LaunchWords { scoped, stack } =
            select::split_scoped_words(&self.launcher, words, joins)?;
        let selection = select::resolve_stack(&self.launcher, &self.loaded, &stack)?;
        let bare = self.compose_stack(&selection)?;
        let framework = self.stack_framework(&selection)?;
        let mut taken = bare.core_nodes.clone();
        let mut copies: Vec<ComposedCopy> = Vec::new();
        // The words scoped to a copy, `NAME.option`, laid over the copy's
        // settings.
        let scoped_words = |name: &Name,
                            loaded: &LoadedOption|
         -> Result<BTreeMap<String, String>, CompositionError> {
            let Some(rests) = scoped.get(name.as_str()) else {
                return Ok(BTreeMap::new());
            };
            select::copy_words(loaded, rests).map_err(|error| as_typed(name, error))
        };
        for entry in &self.launcher.option_deployments {
            let loaded = self.loaded.option(&entry.axis, &entry.option);
            for instance in &entry.instances {
                let settings = entry.settings_for(instance);
                let mut with = settings.with;
                with.extend(scoped_words(&instance.instance_id, loaded)?);
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
        let mut launcher = combine(&self.launcher, &bare, &framework, &copies)?;
        let mut report = CompositionReport {
            selection: selection.clone(),
            copies: Vec::new(),
            applied: bare.applied,
            skipped: bare.skipped,
        };
        for copy in copies {
            let record = copy.record();
            report.add_copy(record, copy.applied, copy.skipped);
        }
        // A copy the command line names is a join: laid over the plan the
        // file's configuration and the joins before it left, under the rules
        // `peppy stack join` holds a copy to.
        for join in joins {
            let joined = self
                .compose_join(
                    JoinRequest {
                        option: &join.option,
                        name: &join.name,
                        words: scoped
                            .get(join.name.as_str())
                            .map_or(&[][..], Vec::as_slice),
                        arguments: &[],
                    },
                    RunningStack {
                        selection: &selection,
                        launcher: &launcher,
                    },
                    CopyOrigin::LaunchJoin,
                )
                .map_err(|error| as_typed(&join.name, error))?;
            launcher = joined.launcher;
            report.add_copy(joined.copy, joined.report.applied, joined.report.skipped);
        }
        // Every copy this launch starts is on the plan now, the file's and
        // the command line's, so this is where a `one_or_more` axis is held
        // to its floor.
        if let Some(axis) = self
            .launcher
            .repeatable_axes()
            .filter(|axis| axis.cardinality == ComponentCardinality::OneOrMore)
            .find(|axis| !report.copies.iter().any(|copy| copy.axis == axis.name))
        {
            return Err(CompositionError::CopyAxisUnfilled {
                axis: axis.name.clone(),
                option: axis
                    .options
                    .keys()
                    .next()
                    .expect("an axis declares at least one option")
                    .clone(),
                menu: select::axes_menu(std::iter::once(axis)),
            });
        }
        Ok(ComposedLaunch {
            launcher,
            selection,
            report,
        })
    }

    /// Composes one more copy over the running stack. The copy starts from
    /// the launcher's entry for its option, as the copies the entry lists
    /// do, with the request's words and arguments on top.
    pub fn join(
        &self,
        request: JoinRequest<'_>,
        stack: RunningStack<'_>,
    ) -> Result<ComposedJoin, CompositionError> {
        self.compose_join(request, stack, CopyOrigin::Join)
    }

    /// One copy joined onto `stack`, `origin` naming the command that asked
    /// for it, which a refusal over an unfilled axis quotes.
    fn compose_join(
        &self,
        request: JoinRequest<'_>,
        stack: RunningStack<'_>,
        origin: CopyOrigin,
    ) -> Result<ComposedJoin, CompositionError> {
        let axis = self.repeatable_axis_of(request.option)?;
        let loaded = self.loaded.option(&axis, request.option);
        let settings = self.joined_copy_settings(
            &axis,
            request.option,
            &select::copy_words(loaded, request.words)?,
            &argument_overrides(request.arguments)?,
        );
        let bare = self.compose_stack(stack.selection)?;
        let copy = compose_copy(
            self,
            stack.selection,
            &bare,
            CopyRequest {
                axis: &axis,
                loaded,
                name: request.name,
                with: &settings.with,
                arguments: &settings.arguments,
                adjustments: &settings.adjustments,
                origin,
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

    /// The copy axis `option` belongs to, which a join and a launch-time
    /// join copy it from.
    fn repeatable_axis_of(&self, option: &str) -> Result<String, CompositionError> {
        let mut repeatable = self.launcher.repeatable_axes().peekable();
        if repeatable.peek().is_none() {
            return Err(CompositionError::NoRepeatableAxis);
        }
        repeatable
            .find(|axis| axis.options.contains_key(option))
            .map(|axis| axis.name.clone())
            .ok_or_else(|| CompositionError::JoinUnknownOption {
                option: option.to_owned(),
                menu: select::axes_menu(self.launcher.repeatable_axes()),
            })
    }

    /// What a joined copy of `option` selects and writes: the settings of
    /// the launcher's entry for the option, when it deploys one, with the
    /// join's own `with` and `arguments` on top, as a copy the file lists
    /// lays its own over the same entry.
    pub(super) fn joined_copy_settings(
        &self,
        axis: &str,
        option: &str,
        with: &BTreeMap<String, String>,
        arguments: &ArgumentOverrides,
    ) -> CopySettings<'_> {
        self.launcher
            .option_deployments
            .iter()
            .find(|entry| entry.axis == axis && entry.option == option)
            .map(OptionDeployment::entry_settings)
            .unwrap_or_default()
            .overlaid_by(with, arguments, [])
    }

    /// The running stack without one of its copies: its instances gone, and
    /// every slot it had released standing vacant again, as the stack this
    /// launch composed declares it. `remaining` names the copies that stay,
    /// whose own released slots are left as they need them.
    pub fn remove(
        &self,
        existing: &PeppyLauncher,
        copy: &CopyRecord,
        selection: &UnitSelection,
        remaining: &[CopyRecord],
    ) -> Result<PeppyLauncher, CompositionError> {
        let Some(axis) = self
            .launcher
            .repeatable_axes()
            .find(|axis| axis.name == copy.axis && axis.options.contains_key(&copy.option))
        else {
            return Err(CompositionError::CopyOfAnotherLauncher {
                copy: copy.name.to_string(),
                axis: copy.axis.clone(),
                option: copy.option.clone(),
            });
        };
        if axis.cardinality == ComponentCardinality::OneOrMore
            && !remaining.iter().any(|record| record.axis == copy.axis)
        {
            return Err(CompositionError::CopyAxisEmptied {
                copy: copy.name.to_string(),
                axis: copy.axis.clone(),
                option: copy.option.clone(),
            });
        }
        let (flat, bare) = self.flat_stack(selection)?;
        // What the copies that stay have released, recomposed from the
        // records that describe them, so a slot one of them pairs into is
        // not handed back as vacant.
        let mut released = HashSet::new();
        for record in remaining {
            let with: BTreeMap<String, String> = record
                .selection
                .own_axes(&record.axis)
                .filter_map(|entry| {
                    entry
                        .option
                        .as_ref()
                        .map(|option| (entry.axis.clone(), option.clone()))
                })
                .collect();
            let composed = compose_copy(
                self,
                selection,
                &bare,
                CopyRequest {
                    axis: &record.axis,
                    loaded: self.loaded.option(&record.axis, &record.option),
                    name: &record.name,
                    with: &with,
                    arguments: &ArgumentOverrides::default(),
                    adjustments: &[],
                    origin: CopyOrigin::Join,
                },
                &[],
            )?;
            released.extend(copy::released_vacancies(&composed));
        }
        copy::detach(existing, copy, &flat, &released)
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
        let base_origin = self.base_origin();
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

    /// How the report and the refusals name the launcher's own entries.
    pub(super) fn base_origin(&self) -> String {
        format!("{} (base)", self.label)
    }

    /// The clock domains the stack declares: the launcher's own and every
    /// selected fragment's, merged. A copy declares none, so this is the
    /// whole launch's set.
    pub(super) fn stack_framework(
        &self,
        selection: &UnitSelection,
    ) -> Result<LauncherFramework, CompositionError> {
        merge_clocks(
            &self.base_origin(),
            &self.launcher.framework,
            &self.loaded.stack_fragments(selection),
        )
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
            self.stack_framework(selection)?,
        ))?;
        Ok((flat, bare))
    }
}

/// A refusal over a word scoped to the copy `name`, named as the operator
/// typed it, `NAME.rest`.
fn as_typed(name: &Name, error: CompositionError) -> CompositionError {
    let scoped = |word: String| format!("{name}.{word}");
    match error {
        CompositionError::UnknownCopySelection { word, option, menu } => {
            CompositionError::UnknownCopySelection {
                word: scoped(word),
                option,
                menu,
            }
        }
        CompositionError::AmbiguousSelection { word, axes } => {
            CompositionError::AmbiguousSelection {
                word: scoped(word),
                axes,
            }
        }
        other => other,
    }
}

/// One copy axis of a launcher, with every instance id its options can
/// define.
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
