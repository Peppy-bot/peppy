//! A launcher held ready to compose: its fragments read once, and the
//! launch, join and removal it answers.

use super::super::composition::{
    ArgumentOverrides, ComponentCardinality, CopySettings, OptionDeployment,
};
use super::super::types::{LauncherFramework, PeppyLauncher};
use super::adjustments::{Routed, route};
use super::check::{TrialRefusal, compose_trial_join, pick_checked_copy_name};
use super::constraints::{self, ConstraintScope};
use super::copy::{
    self, ComposedCopy, CopyRecord, CopyRequest, argument_overrides, combine, compose_copy,
    flat_document, validate_flat,
};
use super::error::CompositionError;
use super::expand::{Expanded, OriginatedDeployment, Unit, expand_unit, merge_clocks};
use super::load::{LoadedComposition, LoadedOption, launcher_file_label, load_composition};
use super::report::{CompositionReport, SkipReason, SkippedAdjustment};
use super::select::{self, CopyOrigin, LaunchWords, PartMachines, UnitSelection, WordOrigin};
use config::runtime::{CopyName, Name};
use core_node_api::encoding::{
    ArgumentOverride, AxisSelection, CopyReference, LaunchWord, SelectionWord,
};
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
    /// The copies the launch starts, the file's in the order it lists them,
    /// then the `--join` ones in the order they were given.
    pub fn copies(&self) -> &[CopyRecord] {
        &self.report.copies
    }

    /// The machine of each placed part of the copy `copy`, by own axis.
    pub fn part_machines_of(&self, copy: &CopyName) -> &PartMachines {
        self.report.parts_of(copy)
    }
}

/// A composed join: the running stack with one more copy.
#[derive(Debug)]
pub struct ComposedJoin {
    pub launcher: PeppyLauncher,
    pub copy: CopyRecord,
    pub report: CompositionReport,
}

impl ComposedJoin {
    /// The machine of each placed part of the joined copy, by own axis.
    pub fn part_machines(&self) -> &PartMachines {
        self.report.parts_of(&self.copy.name)
    }
}

/// What `peppy stack join` asks for: the option to copy, the copy's name,
/// the `--with` words over the option's own axes, each with the machine of
/// the part it selects, and its `--set-arguments` overrides.
#[derive(Debug, Clone, Copy)]
pub struct JoinRequest<'a> {
    pub option: &'a Name,
    pub name: &'a CopyName,
    pub words: &'a [SelectionWord],
    pub arguments: &'a [ArgumentOverride],
}

/// The bare stack of one stack selection, composed once, onto which a
/// launch's scope checks join trial copies, each as `peppy stack join`
/// composes it over a stack that holds no copy.
pub struct TrialStack<'a> {
    prepared: &'a PreparedLauncher,
    selection: &'a UnitSelection,
    flat: PeppyLauncher,
    bare: Expanded,
    routed: Vec<Routed<'a>>,
    name: CopyName,
}

impl TrialStack<'_> {
    /// A copy of `option` joined onto the bare stack with `words`, under a
    /// name the stack does not use. Returns the copy's filled own axes.
    pub fn join(
        &self,
        option: &Name,
        words: &[SelectionWord],
    ) -> Result<Vec<AxisSelection>, CompositionError> {
        let option = option.as_str();
        let axis = self.prepared.repeatable_axis_of(option)?;
        let loaded = self.prepared.loaded.option(&axis, option);
        let words = select::copy_words(loaded, &self.name, WordOrigin::Join, words)?;
        let settings = self.prepared.joined_copy_settings(
            &axis,
            option,
            &words.with,
            &ArgumentOverrides::default(),
        );
        let selection = compose_trial_join(
            self.prepared,
            self.selection,
            &self.flat,
            &self.bare,
            &self.routed,
            &axis,
            loaded,
            &self.name,
            &settings,
        )
        .map_err(|refusal| match refusal {
            TrialRefusal::Composed(error) | TrialRefusal::Joined(error) => *error,
        })?;
        Ok(selection.axis_selections(&axis))
    }
}

/// Why one option of an own axis of a copy option is not a part a word can
/// place on another machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartRefusal {
    /// The option runs on no axis that runs as copies; the join's refusal.
    NotACopyOption(String),
    /// The copy option has no own axis of that name; its own axes.
    NoOwnAxis { axes: Vec<String> },
    /// The own axis has no option of that name; its options.
    NoOption { options: Vec<String> },
    /// The option brings no instance.
    BringsNothing,
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
        words: &[LaunchWord],
        joins: &[CopyReference],
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
        let routed = self.routed();
        // The words scoped to a copy, `NAME.option[@CORE_NODE]`: their options
        // laid over the copy's settings, their machines placing the parts.
        let scoped_words = |name: &CopyName, loaded: &LoadedOption| {
            select::copy_words(
                loaded,
                name,
                WordOrigin::Launch,
                scoped.get(name).map_or(&[][..], Vec::as_slice),
            )
        };
        for entry in &self.launcher.option_deployments {
            let loaded = self.loaded.option(&entry.axis, &entry.option);
            for instance in &entry.instances {
                let settings = entry.settings_for(instance);
                let words = scoped_words(&instance.instance_id, loaded)?;
                let mut with = settings.with;
                with.extend(words.with);
                let copy = compose_copy(
                    self,
                    &selection,
                    &bare,
                    CopyRequest {
                        axis: &entry.axis,
                        loaded,
                        name: &instance.instance_id,
                        with: &with,
                        parts: &words.parts,
                        arguments: &settings.arguments,
                        adjustments: &settings.adjustments,
                        routed: &routed,
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
            applied: bare.applied,
            skipped: bare.skipped,
            ..CompositionReport::default()
        };
        for copy in copies {
            let record = copy.record();
            report.add_copy(record, copy.part_machines, copy.applied, copy.skipped);
        }
        // A copy the command line names is a join: laid over the plan the
        // file's configuration and the joins before it left, under the rules
        // `peppy stack join` holds a copy to.
        for join in joins {
            let joined = self.compose_join(
                JoinRequest {
                    option: &join.option,
                    name: &join.name,
                    words: scoped.get(&join.name).map_or(&[][..], Vec::as_slice),
                    arguments: &[],
                },
                RunningStack {
                    selection: &selection,
                    launcher: &launcher,
                },
                CopyOrigin::LaunchJoin,
            )?;
            launcher = joined.launcher;
            report.absorb(joined.report);
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

    /// Whether `choice` of the own axis `axis` of `option`, an option of an
    /// axis that runs as copies, is a part a word can place: the axis and
    /// the choice exist, and the choice brings at least one instance. Reads
    /// no composition.
    pub fn check_part(&self, option: &str, axis: &str, choice: &str) -> Result<(), PartRefusal> {
        let parent = self
            .repeatable_axis_of(option)
            .map_err(|error| PartRefusal::NotACopyOption(error.to_string()))?;
        let loaded = self.loaded.option(&parent, option);
        let Some(own) = loaded.axis(axis) else {
            return Err(PartRefusal::NoOwnAxis {
                axes: loaded.axes.iter().map(|axis| axis.name.clone()).collect(),
            });
        };
        if !own.options.contains_key(choice) {
            return Err(PartRefusal::NoOption {
                options: own.options.keys().cloned().collect(),
            });
        }
        if !loaded.brings_instances(axis, choice) {
            return Err(PartRefusal::BringsNothing);
        }
        Ok(())
    }

    /// The bare stack of `selection`, composed once, for trial joins.
    pub fn trial_stack<'a>(
        &'a self,
        selection: &'a UnitSelection,
    ) -> Result<TrialStack<'a>, CompositionError> {
        let (flat, bare) = self.flat_stack(selection)?;
        let name = pick_checked_copy_name(&bare);
        Ok(TrialStack {
            prepared: self,
            selection,
            flat,
            bare,
            routed: self.routed(),
            name,
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
        let option = request.option.as_str();
        let axis = self.repeatable_axis_of(option)?;
        let loaded = self.loaded.option(&axis, option);
        let word_origin = match origin {
            CopyOrigin::LaunchJoin => WordOrigin::Launch,
            CopyOrigin::File | CopyOrigin::Join => WordOrigin::Join,
        };
        let words = select::copy_words(loaded, request.name, word_origin, request.words)?;
        let settings = self.joined_copy_settings(
            &axis,
            option,
            &words.with,
            &argument_overrides(request.arguments)?,
        );
        let bare = self.compose_stack(stack.selection)?;
        let routed = self.routed();
        let copy = compose_copy(
            self,
            stack.selection,
            &bare,
            CopyRequest {
                axis: &axis,
                loaded,
                name: request.name,
                with: &settings.with,
                parts: &words.parts,
                arguments: &settings.arguments,
                adjustments: &settings.adjustments,
                routed: &routed,
                origin,
            },
            &stack.launcher.core_nodes,
        )?;
        let launcher = copy::attach(stack.launcher, &copy)?;
        let record = copy.record();
        let mut report = CompositionReport {
            selection: stack.selection.clone(),
            ..CompositionReport::default()
        };
        report.add_copy(
            record.clone(),
            copy.part_machines,
            copy::against_running(stack.launcher, copy.applied),
            copy.skipped,
        );
        Ok(ComposedJoin {
            launcher,
            copy: record,
            report,
        })
    }

    /// The copy axis `option` belongs to, which a join and a launch-time
    /// join copy it from. The refusal is the one a join gives, with the menu
    /// of the options a join can add.
    pub(crate) fn repeatable_axis_of(&self, option: &str) -> Result<String, CompositionError> {
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
    /// whose own released slots are left as they need them. They compose
    /// again from their records, with no part machine: their instances keep
    /// the placements the launch or their join gave them.
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
        let routed = self.routed();
        for record in remaining {
            let with = record.selection.own_choices(&record.axis);
            let composed = compose_copy(
                self,
                selection,
                &bare,
                CopyRequest {
                    axis: &record.axis,
                    loaded: self.loaded.option(&record.axis, &record.option),
                    name: &record.name,
                    with: &with,
                    parts: &PartMachines::new(),
                    arguments: &ArgumentOverrides::default(),
                    adjustments: &[],
                    routed: &routed,
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
        // The stack's selection holds no axis that runs as copies, so an
        // entry under such an option is never part of the stack; an entry
        // reaching the copies from an option that fills once or the top
        // level is reported here, one line per axis.
        let routed = self.routed();
        let mut launcher_adjustments = Vec::new();
        let mut in_copies = Vec::new();
        for routed in routed
            .iter()
            .filter(|routed| routed.entry.selected_in(selection))
        {
            if routed.copy_axes.is_empty() {
                launcher_adjustments.push(routed.entry.originated());
            }
            in_copies.extend(routed.copy_axes.iter().map(|axis| SkippedAdjustment {
                target: routed.entry.adjustment.target.to_string(),
                reason: SkipReason::RunsInCopies((*axis).to_owned()),
                origin: routed.entry.origin.clone(),
            }));
        }
        let deployments_origin = self.top_level_origin("deployments");
        let unit = Unit {
            base: self
                .launcher
                .deployments
                .iter()
                .map(|deployment| OriginatedDeployment {
                    deployment,
                    origin: deployments_origin.clone(),
                })
                .collect(),
            fragments,
            launcher_adjustments,
            copy_adjustments: Vec::new(),
            selection: selection.clone(),
        };
        let mut expanded = expand_unit(&unit, &self.launcher.core_nodes)?;
        expanded.skipped.extend(in_copies);
        Ok(expanded)
    }

    /// How the report and the refusals name one top-level key of the
    /// launcher itself.
    fn top_level_origin(&self, key: &str) -> String {
        format!("{}, top-level `{key}`", self.label)
    }

    /// The launcher's adjustments in the order they apply, each with the
    /// units that run it.
    pub(super) fn routed(&self) -> Vec<Routed<'_>> {
        let (_, routed) = route(&self.launcher, &self.loaded, &self.label);
        routed
    }

    /// The clock domains the stack declares: the launcher's own and every
    /// selected fragment's, merged. A copy declares none, so this is the
    /// whole launch's set.
    pub(super) fn stack_framework(
        &self,
        selection: &UnitSelection,
    ) -> Result<LauncherFramework, CompositionError> {
        merge_clocks(
            &self.top_level_origin("framework"),
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
