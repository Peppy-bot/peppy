//! `robot_membership:v1`: the copies of the running launch, listed, added and
//! removed. Its scope names the launcher options a client may add, with the
//! robot model and a description of each and the features a copy of it may
//! hold, and the most copies of them the stack holds.

use crate::internal::launcher::{
    CompositionError, JoinableNodes, PartRefusal, PreparedLauncher, UnitSelection,
};
use config::runtime::{CoreNodeName, CoreNodeTarget, Name};
use core_node_api::encoding::{AxisSelection, OptionChoice, SelectionWord};
use peppy_mcp_catalog::ExposureBundle;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::num::NonZeroU16;

/// The interface's document, compiled in.
pub(super) const DOCUMENT: &str = include_str!("robot_membership.v1.json5");

/// The refusal of a scope in a launcher that declares no axis that runs as
/// copies. A flat launcher declares no axis, so the flat document `peppy
/// stack resolve` prints for a launcher with a scope gets it too.
const NO_AXIS_RUNS_AS_COPIES: &str = "this launcher declares no axis that runs as copies \
     (`zero_or_more` or `one_or_more`), so no option of a `robot_membership` scope can be added; a \
     flat launcher, such as the one `peppy stack resolve` prints, declares no axis at all: \
     launch the launcher it was resolved from, or take the instance that serves this target \
     out of the flat file";

/// A member of `robot_membership:v1`, as an exposure entry names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RobotMembershipMember {
    /// The service that answers the scope's options and the copies of them
    /// on the stack.
    List,
    /// The action that adds a copy: its goal names the copy, its option,
    /// its machine and its features.
    Join,
    /// The action that removes a copy: its goal names the copy.
    Remove,
}

impl RobotMembershipMember {
    /// Every member of the interface.
    pub const ALL: [Self; 3] = [Self::List, Self::Join, Self::Remove];

    /// The member's name in the interface's document.
    pub fn name(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Join => "join",
            Self::Remove => "remove",
        }
    }

    /// The member the document names `name`, if any.
    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|member| member.name() == name)
    }
}

/// The most characters a description holds.
pub const MAX_DESCRIPTION_CHARS: usize = 200;

/// The most features one option holds, so a launch composes at most 64
/// feature sets of each option.
pub const MAX_FEATURES: usize = 6;

/// One line for the model about an option or a feature: not blank, one
/// line, at most [`MAX_DESCRIPTION_CHARS`] characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Description(String);

impl Description {
    /// `text` as the description of `what`, which the refusal names.
    fn new(text: String, what: &str) -> Result<Self, String> {
        if text.trim().is_empty() {
            return Err(format!(
                "the description of {what} is empty; it says in one line what it is"
            ));
        }
        if text.chars().any(char::is_control) {
            return Err(format!(
                "the description of {what} is one line, with no line break or other control \
                 character"
            ));
        }
        let length = text.chars().count();
        if length > MAX_DESCRIPTION_CHARS {
            return Err(format!(
                "the description of {what} is at most {MAX_DESCRIPTION_CHARS} characters, and \
                 this one has {length}"
            ));
        }
        Ok(Self(text))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The scope of a daemon target of `robot_membership:v1`: the launcher
/// options a client may add, each once and with its robot model, a
/// description and the features it offers, and the most copies of these
/// options the stack holds. Parsing holds every rule, so a value of this type
/// is a scope the server can use.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawRobotMembershipScope")]
pub struct RobotMembershipScope {
    options: Vec<ScopedOption>,
    max_copies: NonZeroU16,
}

/// One launcher option a client may add a copy of, the robot model a copy
/// of it is, and the features a copy of it may hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedOption {
    option: Name,
    model: Name,
    description: Description,
    features: Vec<ScopedFeature>,
}

/// One feature of a scoped option: a name, the one option of one own axis
/// of the scoped option that a robot with the feature holds, and one line
/// that says what the feature adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedFeature {
    feature: Name,
    axis: Name,
    option: Name,
    description: Description,
}

/// A feature that a robot's selections hold, with the machine of its part
/// when that part runs away from the robot's machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacedFeature<'a> {
    pub feature: &'a ScopedFeature,
    pub core_node: Option<&'a CoreNodeName>,
}

/// Wire shape of [`RobotMembershipScope`]. `max_copies` is read as any number
/// so that a value outside its range is refused with the range.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRobotMembershipScope {
    options: Vec<RawScopedOption>,
    max_copies: serde_json::Number,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScopedOption {
    option: Name,
    model: Name,
    description: String,
    #[serde(default)]
    features: Option<Vec<RawScopedFeature>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScopedFeature {
    feature: Name,
    axis: Name,
    option: Name,
    description: String,
}

impl TryFrom<RawRobotMembershipScope> for RobotMembershipScope {
    type Error = String;

    fn try_from(raw: RawRobotMembershipScope) -> Result<Self, String> {
        if raw.options.is_empty() {
            return Err(
                "`options` names no option; a scope names at least one launcher option a client \
                 may add"
                    .to_owned(),
            );
        }
        let mut seen = HashSet::with_capacity(raw.options.len());
        for entry in &raw.options {
            if !seen.insert(entry.option.as_str()) {
                return Err(format!(
                    "`options` names option `{}` more than once",
                    entry.option
                ));
            }
        }
        let max_copies = raw
            .max_copies
            .as_u64()
            .and_then(|value| u16::try_from(value).ok())
            .and_then(NonZeroU16::new)
            .ok_or_else(|| {
                format!(
                    "`max_copies` is a whole number from 1 to {}, got {}",
                    u16::MAX,
                    raw.max_copies
                )
            })?;
        Ok(Self {
            options: raw
                .options
                .into_iter()
                .map(ScopedOption::try_from)
                .collect::<Result<_, _>>()?,
            max_copies,
        })
    }
}

impl TryFrom<RawScopedOption> for ScopedOption {
    type Error = String;

    fn try_from(raw: RawScopedOption) -> Result<Self, String> {
        let option = raw.option;
        let description = Description::new(raw.description, &format!("option `{option}`"))?;
        let features = match raw.features {
            None => Vec::new(),
            Some(features) if features.is_empty() => {
                return Err(format!(
                    "`features` of option `{option}` lists no feature; leave `features` out for \
                     an option with none"
                ));
            }
            Some(features) if features.len() > MAX_FEATURES => {
                return Err(format!(
                    "option `{option}` lists {} features, and an option holds at most \
                     {MAX_FEATURES}",
                    features.len()
                ));
            }
            Some(features) => features
                .into_iter()
                .map(|feature| {
                    let what = format!("feature `{}` of option `{option}`", feature.feature);
                    Ok(ScopedFeature {
                        description: Description::new(feature.description, &what)?,
                        feature: feature.feature,
                        axis: feature.axis,
                        option: feature.option,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?,
        };
        for (index, first) in features.iter().enumerate() {
            for second in &features[index + 1..] {
                if first.feature == second.feature {
                    return Err(format!(
                        "option `{option}` lists feature `{}` twice, on axis `{}` and on axis \
                         `{}`; give each feature its own name",
                        first.feature, first.axis, second.axis
                    ));
                }
                if first.axis == second.axis {
                    return Err(format!(
                        "features `{}` and `{}` of option `{option}` both select axis `{}`; a \
                         robot holds one option on an axis, so an axis has at most one feature",
                        first.feature, second.feature, first.axis
                    ));
                }
            }
        }
        Ok(Self {
            option,
            model: raw.model,
            description,
            features,
        })
    }
}

impl ScopedOption {
    /// The option of an axis that runs as copies: the value `join` takes.
    pub fn option(&self) -> &Name {
        &self.option
    }

    /// The robot model a copy of the option is, as the scope names it.
    pub fn model(&self) -> &Name {
        &self.model
    }

    /// One line that says what a copy of the option is.
    pub fn description(&self) -> &Description {
        &self.description
    }

    /// The features a copy of the option may hold, in the scope's order.
    pub fn features(&self) -> &[ScopedFeature] {
        &self.features
    }

    /// The feature named `name`, if the option offers it.
    pub fn feature(&self, name: &str) -> Option<&ScopedFeature> {
        self.features
            .iter()
            .find(|feature| feature.feature.as_str() == name)
    }

    /// The features that a robot of this option holds, given its filled own
    /// axes: each feature whose axis its selections fill with the feature's
    /// option, with the machine of its part, in the scope's order.
    pub fn features_in<'a>(&'a self, selections: &'a [AxisSelection]) -> Vec<PlacedFeature<'a>> {
        self.features
            .iter()
            .filter_map(|feature| {
                let selection = selections.iter().find(|selection| {
                    selection.axis == feature.axis && selection.option == feature.option
                })?;
                Some(PlacedFeature {
                    feature,
                    core_node: selection.core_node.as_ref(),
                })
            })
            .collect()
    }
}

impl ScopedFeature {
    /// The feature's name, the value a `join` feature item takes.
    pub fn feature(&self) -> &Name {
        &self.feature
    }

    /// The own axis of the scoped option the feature fills.
    pub fn axis(&self) -> &Name {
        &self.axis
    }

    /// The option of [`Self::axis`] a robot with the feature holds.
    pub fn option(&self) -> &Name {
        &self.option
    }

    /// One line that says what the feature adds.
    pub fn description(&self) -> &Description {
        &self.description
    }

    /// The selection word that adds the feature, with the axis named, its
    /// part on `core_node`, or with the robot's base when `None`.
    pub fn word(&self, core_node: Option<CoreNodeTarget>) -> SelectionWord {
        SelectionWord {
            choice: OptionChoice {
                axis: Some(self.axis.clone()),
                option: self.option.clone(),
            },
            core_node,
        }
    }
}

impl RobotMembershipScope {
    /// The options a client may add, in the order the launcher lists them.
    pub fn options(&self) -> &[ScopedOption] {
        &self.options
    }

    /// The scoped option `option`, if the scope names it.
    pub fn option(&self, option: &str) -> Option<&ScopedOption> {
        self.options
            .iter()
            .find(|entry| entry.option.as_str() == option)
    }

    /// The most copies of the scope's options the stack holds.
    pub fn max_copies(&self) -> NonZeroU16 {
        self.max_copies
    }

    /// The scope against the launcher and the stack `selection` resolves,
    /// in three steps, each run only when the steps before it hold:
    /// 1. every scoped option is an option of an axis that runs as copies,
    ///    with one refusal for a launcher with no such axis;
    /// 2. every feature selects an option of an own axis of its option, and
    ///    that option brings an instance;
    /// 3. every feature set of every option (no feature, each feature, each
    ///    combination) composes as a join over the bare stack, and the robot
    ///    with no feature holds none of its option's features.
    ///
    /// The verdict depends only on the launcher and the stack selection.
    /// When the scope holds, returns the nodes of the trial copies of step 3:
    /// every node that a join of a scoped option can bring.
    pub(super) fn check_launch(
        &self,
        prepared: &PreparedLauncher,
        selection: &UnitSelection,
    ) -> Result<JoinableNodes, Vec<String>> {
        let errors: Vec<CompositionError> = self
            .options
            .iter()
            .filter_map(|entry| prepared.repeatable_axis_of(entry.option.as_str()).err())
            .collect();
        if errors
            .iter()
            .any(|error| matches!(error, CompositionError::NoRepeatableAxis))
        {
            return Err(vec![NO_AXIS_RUNS_AS_COPIES.to_owned()]);
        }
        if !errors.is_empty() {
            // The join's own refusal names the option and the menu.
            return Err(errors.iter().map(ToString::to_string).collect());
        }
        let features: Vec<String> = self
            .options
            .iter()
            .flat_map(|entry| {
                entry
                    .features
                    .iter()
                    .filter_map(|feature| check_feature(prepared, entry, feature).err())
            })
            .collect();
        if !features.is_empty() {
            return Err(features);
        }
        self.check_trial_joins(prepared, selection)
    }

    /// Step 3 of [`Self::check_launch`]: one trial join of each feature set
    /// of each option over the bare stack, whose nodes are the joinable
    /// nodes of the scope.
    fn check_trial_joins(
        &self,
        prepared: &PreparedLauncher,
        selection: &UnitSelection,
    ) -> Result<JoinableNodes, Vec<String>> {
        let trials = prepared
            .trial_stack(selection)
            .map_err(|error| vec![format!("the stack does not compose: {error}")])?;
        let mut joinable = JoinableNodes::default();
        let mut refusals = Vec::new();
        for entry in &self.options {
            for set in feature_sets(&entry.features) {
                let words: Vec<SelectionWord> =
                    set.iter().map(|feature| feature.word(None)).collect();
                let trial = match trials.join(&entry.option, &words) {
                    Ok(trial) => trial,
                    Err(error) => {
                        refusals.push(format!("{}: {error}", feature_set_label(entry, &set)));
                        continue;
                    }
                };
                joinable.add_copy(&entry.option, trial.deployments);
                if !set.is_empty() {
                    continue;
                }
                let held = entry.features_in(&trial.selections);
                if !held.is_empty() {
                    refusals.push(format!(
                        "option `{}` with no feature already holds the {} {}: its launcher \
                         entry selects them, so a feature adds nothing to its robot; take them \
                         out of the scope, or out of the entry",
                        entry.option,
                        if held.len() == 1 {
                            "feature"
                        } else {
                            "features"
                        },
                        crate::error::format_quoted_list(
                            held.iter().map(|placed| placed.feature.feature.as_str())
                        )
                    ));
                }
            }
        }
        if refusals.is_empty() {
            Ok(joinable)
        } else {
            Err(refusals)
        }
    }

    /// `join.option` takes only the scope's options; `join.name`,
    /// `join.core_node`, the `core_node` of each feature item and
    /// `remove.name` only a core node name, the name rule the daemon and the
    /// CLI apply to a copy and a machine; and the `feature` of a feature
    /// item only a feature of the scope. A scope with no feature takes an
    /// empty `features` only. The machines of the federation change while
    /// the endpoint runs, so no schema lists them.
    pub(super) fn narrow(&self, bundle: &mut ExposureBundle, target: &str) {
        let name_rule = CoreNodeName::json_schema();
        let options: Vec<&str> = self
            .options
            .iter()
            .map(|entry| entry.option.as_str())
            .collect();
        let mut features: Vec<&str> = Vec::new();
        for feature in self.options.iter().flat_map(|entry| &entry.features) {
            if !features.contains(&feature.feature.as_str()) {
                features.push(feature.feature.as_str());
            }
        }
        for task in bundle.tasks.iter_mut().filter(|task| task.target == target) {
            let Some(properties) = task
                .input_schema
                .get_mut("properties")
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            match RobotMembershipMember::named(&task.member) {
                Some(RobotMembershipMember::Join) => {
                    properties.insert(
                        "option".to_owned(),
                        serde_json::json!({ "type": "string", "enum": options }),
                    );
                    properties.insert("name".to_owned(), name_rule.clone());
                    properties.insert("core_node".to_owned(), name_rule.clone());
                    narrow_features(properties, &features, &name_rule);
                }
                Some(RobotMembershipMember::Remove) => {
                    properties.insert("name".to_owned(), name_rule.clone());
                }
                Some(RobotMembershipMember::List) | None => {}
            }
        }
    }
}

/// Narrows the `features` property of the join input: each item's `feature`
/// to `features` and its `core_node` to `name_rule`, written into the
/// item's own properties so the item keeps its `required` list and its
/// `additionalProperties: false`. With no feature in the scope, `features`
/// takes an empty array only.
fn narrow_features(
    properties: &mut serde_json::Map<String, Value>,
    features: &[&str],
    name_rule: &Value,
) {
    if features.is_empty() {
        properties.insert(
            "features".to_owned(),
            serde_json::json!({ "type": "array", "maxItems": 0 }),
        );
        return;
    }
    let Some(item) = properties
        .get_mut("features")
        .and_then(|schema| schema.get_mut("items"))
        .and_then(|items| items.get_mut("properties"))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    item.insert(
        "feature".to_owned(),
        serde_json::json!({ "type": "string", "enum": features }),
    );
    item.insert("core_node".to_owned(), name_rule.clone());
}

/// Step 2 of [`RobotMembershipScope::check_launch`] for one feature, with no
/// composition.
fn check_feature(
    prepared: &PreparedLauncher,
    entry: &ScopedOption,
    feature: &ScopedFeature,
) -> Result<(), String> {
    let selects = format!(
        "feature `{}` of option `{}` selects `{}={}`",
        feature.feature, entry.option, feature.axis, feature.option
    );
    prepared
        .check_part(
            entry.option.as_str(),
            feature.axis.as_str(),
            feature.option.as_str(),
        )
        .map_err(|refusal| match refusal {
            PartRefusal::NotACopyOption(reason) => format!("{selects}: {reason}"),
            PartRefusal::NoOwnAxis { axes } => format!(
                "{selects}, but `{}` has no own axis `{}`; its own axes: {}",
                entry.option,
                feature.axis,
                if axes.is_empty() {
                    String::from("none")
                } else {
                    crate::error::format_quoted_list(&axes)
                }
            ),
            PartRefusal::NoOption { options } => format!(
                "{selects}, but axis `{}` of `{}` has no option `{}`; its options: {}",
                feature.axis,
                entry.option,
                feature.option,
                crate::error::format_quoted_list(&options)
            ),
            PartRefusal::BringsNothing => format!(
                "{selects}, which brings no instance, so it cannot run on another machine and \
                 cannot be a feature"
            ),
        })
}

/// Every set of `features`: no feature, then each feature, then each
/// combination, smaller sets first, each in the scope's order.
fn feature_sets(features: &[ScopedFeature]) -> Vec<Vec<&ScopedFeature>> {
    let mut masks: Vec<u32> = (0..1u32 << features.len()).collect();
    masks.sort_by_key(|mask| {
        let members: Vec<usize> = (0..features.len())
            .filter(|index| mask & (1 << index) != 0)
            .collect();
        (members.len(), members)
    });
    masks
        .into_iter()
        .map(|mask| {
            features
                .iter()
                .enumerate()
                .filter(|(index, _)| mask & (1 << index) != 0)
                .map(|(_, feature)| feature)
                .collect()
        })
        .collect()
}

/// How a trial refusal names its option and feature set.
fn feature_set_label(entry: &ScopedOption, set: &[&ScopedFeature]) -> String {
    match set {
        [] => format!("option `{}` with no feature", entry.option),
        _ => format!(
            "option `{}` with the features {}",
            entry.option,
            crate::error::format_quoted_list(set.iter().map(|feature| feature.feature.as_str()))
        ),
    }
}
