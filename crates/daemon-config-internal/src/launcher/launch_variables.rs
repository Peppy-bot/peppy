//! Launch variables: the environment variables that an instance takes from
//! the environment of the launch, by name. A launcher names them in the
//! `env_from_launch` of an instance and with a `set_env_from_launch`
//! adjustment, so no launcher file holds a value: a list, or an `any_of` of
//! sets ([`LaunchVariables`]).
//!
//! A launch and a join parse the environment that the CLI forwarded with
//! them against the launch variables of the launcher ([`ForwardedEnv`]): the
//! value of a launch variable reaches the run of each instance that takes it,
//! and no other node, add or build. The daemon keeps, in memory, the values
//! of the launch ([`LaunchValues`]). An instance takes the values of a
//! change ([`ChangeValues`]) as [`LaunchVariables::values_taken`] tells:
//! each variable of its list, from the forwarded environment of the change,
//! else from the kept values; or the first set of its `any_of` that the
//! forwarded environment of the change holds complete, else the first set
//! that the kept values hold complete, every value of the set from that one
//! source.
//!
//! The launch is refused when an instance that the launch or a join through
//! a `robot_membership` scope can start takes a variable that the forwarded
//! environment of the launch does not hold, or an `any_of` none of whose
//! sets it holds complete; a join, when an instance that it starts takes a
//! variable that neither its own forwarded environment nor the kept values
//! hold, or an `any_of` none of whose sets one of them holds complete
//! ([`VariableNeeds::check`]). A launch and a join also refuse a set that
//! their own forwarded environment holds in part, so that an instance never
//! takes another set in its place. The launch refuses a set held in part of
//! each `any_of` of its launcher, also when no instance that it can start
//! takes that `any_of` ([`refuse_kept_sets_held_in_part`]): it keeps the
//! values for each later join, so the kept values never hold a set in part.
//! A launcher names a variable of a set of two variables or more in no list
//! and in no other set ([`refuse_variables_outside_their_set`]).
//! An instance with launch variables runs on the coordinator's machine only
//! ([`refuse_peer_placements`]).

use super::types::{DeploymentInstance, Placements};
use crate::internal::env::{InvalidEnvVar, SAFE_ENV_VALUE_CHARACTERS, check_launch_variable_name};
use config::runtime::Name;
use serde::{Deserialize, Serialize};
use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet};

/// The name of one variable that an instance takes from the launch
/// environment: a shell identifier, not a forbidden key, and not a name
/// that the CLI never forwards ([`check_launch_variable_name`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LaunchVariable(String);

impl LaunchVariable {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for LaunchVariable {
    type Error = InvalidEnvVar;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        check_launch_variable_name(&name)?;
        Ok(Self(name))
    }
}

impl From<LaunchVariable> for String {
    fn from(variable: LaunchVariable) -> Self {
        variable.0
    }
}

/// A map or a set of launch variables is read by the name alone: the order,
/// the equality and the hash of a variable are those of its name.
impl Borrow<str> for LaunchVariable {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for LaunchVariable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A list of launch variables that names one variable two times.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the list names `{variable}` two times; a list of launch variables names each one once")]
pub struct RepeatedLaunchVariable {
    pub variable: LaunchVariable,
}

/// An `any_of` that names no set an instance can take, or a set that an
/// instance never takes. Each message names a set by its place in the list,
/// from 1.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrokenAnyOf {
    #[error(
        "`any_of` names no set; an instance that takes no variable from the launch writes `[]`"
    )]
    NoSet,
    #[error("`any_of` names one set; write that set as a list")]
    OneSet,
    #[error("set {set} of `any_of` is empty; each set names at least one variable")]
    EmptySet { set: usize },
    #[error("set {set} of `any_of` names `{variable}` two times; a set names each variable once")]
    RepeatedInSet {
        set: usize,
        variable: LaunchVariable,
    },
    /// Two sets that share a variable: when the later one is complete, the
    /// earlier one is complete before it, or held in part, which a launch
    /// and a join refuse. Two equal sets, and a set that holds another, are
    /// two such sets.
    #[error(
        "sets {earlier} and {later} of `any_of` both name `{variable}`, so set {later} is never \
         taken: when the environment holds it complete, it holds set {earlier} complete too, or \
         in part, which is refused; write each variable in one set only"
    )]
    SharedVariable {
        earlier: usize,
        later: usize,
        variable: LaunchVariable,
    },
}

/// The key of the `any_of` form of a list of launch variables.
const ANY_OF: &str = "any_of";

/// The variables that one instance takes from the launch environment, in one
/// of two forms.
///
/// A list, `["A", "B"]`, names variables that the instance takes each one
/// of. It is a set: two lists that name the same variables in another order
/// are the same, and a list writes back in name order. An empty list is the
/// same as no list.
///
/// An `any_of`, `{ any_of: [["A", "B"], ["C"]] }`, names two sets or more, in
/// order: of these, [`Self::taken`] gives the first set that the environment
/// holds complete. Each variable is in one set only. Each set writes back in
/// name order, and the sets in the order written. The parse refuses an
/// `any_of` that [`BrokenAnyOf`] names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchVariables(Form);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Form {
    /// A list: each variable is taken.
    Each(BTreeSet<LaunchVariable>),
    /// An `any_of`: the first complete set is taken.
    AnyOf(Vec<BTreeSet<LaunchVariable>>),
}

impl Default for Form {
    fn default() -> Self {
        Self::Each(BTreeSet::new())
    }
}

/// The variables of `written` as a set, or the first variable that it names
/// a second time.
fn set_of(written: Vec<LaunchVariable>) -> Result<BTreeSet<LaunchVariable>, LaunchVariable> {
    let mut variables = BTreeSet::new();
    for variable in written {
        if variables.contains(&variable) {
            return Err(variable);
        }
        variables.insert(variable);
    }
    Ok(variables)
}

/// The first set of `earlier_sets` that names a variable of `set`, by its
/// place from 1, and the first such variable in name order.
fn first_shared_variable<'a>(
    earlier_sets: &[BTreeSet<LaunchVariable>],
    set: &'a BTreeSet<LaunchVariable>,
) -> Option<(usize, &'a LaunchVariable)> {
    earlier_sets
        .iter()
        .enumerate()
        .find_map(|(index, earlier)| {
            set.iter()
                .find(|variable| earlier.contains(*variable))
                .map(|variable| (index + 1, variable))
        })
}

/// Whether `values` hold every variable of `set`.
fn holds_complete(set: &BTreeSet<LaunchVariable>, values: &LaunchValues) -> bool {
    set.iter().all(|variable| values.holds(variable.as_str()))
}

impl TryFrom<Vec<LaunchVariable>> for LaunchVariables {
    type Error = RepeatedLaunchVariable;

    fn try_from(written: Vec<LaunchVariable>) -> Result<Self, Self::Error> {
        set_of(written)
            .map(|variables| Self(Form::Each(variables)))
            .map_err(|variable| RepeatedLaunchVariable { variable })
    }
}

impl LaunchVariables {
    /// The `any_of` of `written`, its sets in order: two sets or more, none
    /// empty, each naming a variable once, and no variable in two sets.
    pub fn any_of(written: Vec<Vec<LaunchVariable>>) -> Result<Self, BrokenAnyOf> {
        match written.len() {
            0 => return Err(BrokenAnyOf::NoSet),
            1 => return Err(BrokenAnyOf::OneSet),
            _ => {}
        }
        let mut sets: Vec<BTreeSet<LaunchVariable>> = Vec::with_capacity(written.len());
        for (index, variables) in written.into_iter().enumerate() {
            let set = index + 1;
            if variables.is_empty() {
                return Err(BrokenAnyOf::EmptySet { set });
            }
            let variables = set_of(variables)
                .map_err(|variable| BrokenAnyOf::RepeatedInSet { set, variable })?;
            if let Some((earlier, variable)) = first_shared_variable(&sets, &variables) {
                return Err(BrokenAnyOf::SharedVariable {
                    earlier,
                    later: set,
                    variable: variable.clone(),
                });
            }
            sets.push(variables);
        }
        Ok(Self(Form::AnyOf(sets)))
    }

    /// Whether the instance takes no variable: an empty list.
    pub fn is_empty(&self) -> bool {
        matches!(&self.0, Form::Each(variables) if variables.is_empty())
    }

    /// Every variable that the instance can take, each once, in name order:
    /// each variable of a list, or the variables of every set of an `any_of`.
    pub fn iter(&self) -> impl Iterator<Item = &LaunchVariable> {
        let names: BTreeSet<&LaunchVariable> = match &self.0 {
            Form::Each(variables) => variables.iter().collect(),
            Form::AnyOf(sets) => sets.iter().flatten().collect(),
        };
        names.into_iter()
    }

    /// The variables that the instance takes each one of, in name order: the
    /// variables of a list, and none for an `any_of`.
    pub fn required(&self) -> impl Iterator<Item = &LaunchVariable> {
        let list = match &self.0 {
            Form::Each(variables) => Some(variables),
            Form::AnyOf(_) => None,
        };
        list.into_iter().flatten()
    }

    /// The sets of an `any_of`, in order, and none for a list.
    pub fn sets(&self) -> &[BTreeSet<LaunchVariable>] {
        match &self.0 {
            Form::Each(_) => &[],
            Form::AnyOf(sets) => sets,
        }
    }

    /// The first set of an `any_of` that `values` hold complete. A list has
    /// no set.
    fn first_set_complete_in(&self, values: &LaunchValues) -> Option<&BTreeSet<LaunchVariable>> {
        self.sets().iter().find(|set| holds_complete(set, values))
    }

    /// The value of each variable that the instance takes, in name order,
    /// from `own`, the values of the change that starts it, and `kept`, the
    /// values that the launch kept. Each variable of a list takes its value
    /// from `own`, else from `kept`, and a variable that neither holds is
    /// left out. An `any_of` takes the first set that `own` holds complete,
    /// with the values of `own`, else the first set that `kept` holds
    /// complete, with the values of `kept`, and nothing when neither holds a
    /// set complete: the values of a set come from one source, never from
    /// the two.
    pub fn values_taken<'a>(
        &'a self,
        own: &'a LaunchValues,
        kept: &'a LaunchValues,
    ) -> Vec<(&'a LaunchVariable, &'a str)> {
        let Form::AnyOf(_) = &self.0 else {
            return self
                .required()
                .filter_map(|variable| {
                    let name = variable.as_str();
                    let value = own.value_of(name).or_else(|| kept.value_of(name))?;
                    Some((variable, value))
                })
                .collect();
        };
        let taken = self
            .first_set_complete_in(own)
            .map(|set| (set, own))
            .or_else(|| self.first_set_complete_in(kept).map(|set| (set, kept)));
        let Some((set, source)) = taken else {
            return Vec::new();
        };
        set.iter()
            .filter_map(|variable| Some((variable, source.value_of(variable.as_str())?)))
            .collect()
    }

    /// The sets of an `any_of` that `values` hold in part, in order. A set of
    /// one variable is never held in part, and a list has no set.
    fn sets_held_in_part(&self, values: &LaunchValues) -> Vec<SetHeldInPart> {
        self.sets()
            .iter()
            .filter_map(|set| SetHeldInPart::of(set, values))
            .collect()
    }

    /// The variables as a message names them: a list as `` `A`, `B` ``, and
    /// an `any_of` as "one set of `A` and `B`, or of `C`".
    pub fn describe(&self) -> String {
        match &self.0 {
            Form::Each(variables) => crate::error::format_quoted_list(variables),
            Form::AnyOf(sets) => {
                let sets: Vec<String> = sets.iter().map(quoted_set).collect();
                format!("one set of {}", sets.join(", or of "))
            }
        }
    }

    /// Every variable that the instance can take ([`Self::iter`]) that
    /// `env_vars` also gives a value, in name order: each one would get two
    /// values.
    pub fn given_in<'a>(
        &'a self,
        env_vars: &'a BTreeMap<String, String>,
    ) -> impl Iterator<Item = &'a LaunchVariable> + 'a {
        self.iter()
            .filter(|variable| env_vars.contains_key(variable.as_str()))
    }
}

/// A list writes as a list of names, and an `any_of` as an object with the
/// one key `any_of`.
impl Serialize for LaunchVariables {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match &self.0 {
            Form::Each(variables) => serializer.collect_seq(variables),
            Form::AnyOf(sets) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry(ANY_OF, sets)?;
                map.end()
            }
        }
    }
}

/// A list reads as [`LaunchVariables::try_from`], and an object as
/// [`LaunchVariables::any_of`]. Each name is a [`LaunchVariable`], and every
/// refusal keeps the text of its error.
impl<'de> Deserialize<'de> for LaunchVariables {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(LaunchVariablesVisitor)
    }
}

/// The object form as it is written: the key `any_of` and no other.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WrittenAnyOf {
    any_of: Vec<Vec<LaunchVariable>>,
}

struct LaunchVariablesVisitor;

impl<'de> serde::de::Visitor<'de> for LaunchVariablesVisitor {
    type Value = LaunchVariables;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a list of variable names, or an object `{ any_of: [[...], [...]] }`")
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
        let written =
            Vec::<LaunchVariable>::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))?;
        LaunchVariables::try_from(written).map_err(serde::de::Error::custom)
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        let WrittenAnyOf { any_of } =
            WrittenAnyOf::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
        LaunchVariables::any_of(any_of).map_err(serde::de::Error::custom)
    }
}

/// One instance that takes `variable` from the launch and also gives it a
/// value in its `env_vars`. `list_origin` names the document or the adjustment
/// that wrote the `env_from_launch` of the instance, when the composition
/// knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableGivenTwoValues {
    pub instance: String,
    pub variable: LaunchVariable,
    pub list_origin: Option<String>,
}

impl VariableGivenTwoValues {
    /// One for each variable that `instance` takes from the launch and also
    /// gives a value in its `env_vars`, its list written by `list_origin`.
    pub(crate) fn each_of<'a>(
        instance: &'a DeploymentInstance,
        list_origin: Option<&'a str>,
    ) -> impl Iterator<Item = Self> + 'a {
        instance
            .env_from_launch
            .given_in(&instance.env_vars)
            .map(move |variable| Self {
                instance: instance.instance_id.to_string(),
                variable: variable.clone(),
                list_origin: list_origin.map(str::to_owned),
            })
    }
}

impl std::fmt::Display for VariableGivenTwoValues {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            instance,
            variable,
            list_origin,
        } = self;
        write!(
            f,
            "instance `{instance}` takes `{variable}` from the launch in `env_from_launch`"
        )?;
        if let Some(origin) = list_origin {
            write!(f, " (set by {origin})")?;
        }
        f.write_str(" and gives it a value in `env_vars`")
    }
}

/// The refusal of the instances that take a variable from the launch and
/// also give it a value in their `env_vars`: each variable would get two
/// values. It names every such instance and variable at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariablesGivenTwoValues(Vec<VariableGivenTwoValues>);

impl VariablesGivenTwoValues {
    /// The refusal of `found`, or none when `found` is empty.
    pub fn refuse(found: Vec<VariableGivenTwoValues>) -> Result<(), Self> {
        if found.is_empty() {
            Ok(())
        } else {
            Err(Self(found))
        }
    }
}

impl std::fmt::Display for VariablesGivenTwoValues {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0.as_slice() {
            [one] => write!(
                f,
                "{one}, so the variable would get two values; take it out of one of the two"
            ),
            several => write!(
                f,
                "these instances take a variable from the launch and give it a value in \
                 `env_vars`, so each of these variables would get two values; take each one out \
                 of one of the two:{}",
                crate::error::format_bulleted(several)
            ),
        }
    }
}

impl std::error::Error for VariablesGivenTwoValues {}

/// What can start an instance that takes launch variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Starter {
    /// The launch starts it.
    Launch,
    /// A join through the `robot_membership` scope of each instance of
    /// `scopes` can add a copy of `option` that starts it, with the features
    /// `features`: none for a copy with no feature, else the smallest set of
    /// features with which a copy starts it.
    ScopedJoin {
        option: Name,
        features: Vec<Name>,
        scopes: Vec<String>,
    },
    /// The join starts it.
    Join,
}

/// One instance that takes one launch variable, and what can start it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableNeed {
    pub variable: LaunchVariable,
    /// The instance: its id in the stack, or its id in the copy for an
    /// instance of a copy that a join through a scope can add.
    pub instance: String,
    pub starter: Starter,
}

impl VariableNeed {
    /// One need for each variable of the list of `instance`, `instance` named
    /// `shown`. An `any_of` is a [`ChoiceNeed`].
    pub fn each_of<'a>(
        shown: &'a str,
        instance: &'a DeploymentInstance,
        starter: &'a Starter,
    ) -> impl Iterator<Item = Self> + 'a {
        instance
            .env_from_launch
            .required()
            .map(move |variable| Self {
                variable: variable.clone(),
                instance: shown.to_owned(),
                starter: starter.clone(),
            })
    }
}

/// The instance that takes the variable and what can start it, as a clause:
/// "`brain_inst` takes it from the launch, and the launch starts it".
impl std::fmt::Display for VariableNeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_need(f, &self.instance, &self.starter, "it")
    }
}

/// One instance that takes the first complete set of an `any_of`, and what
/// can start it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceNeed {
    /// The `any_of` that the instance takes a set of.
    pub variables: LaunchVariables,
    /// The instance: its id in the stack, or its id in the copy for an
    /// instance of a copy that a join through a scope can add.
    pub instance: String,
    pub starter: Starter,
}

impl ChoiceNeed {
    /// The need of `instance`, named `shown`, when it takes an `any_of`.
    pub fn of(shown: &str, instance: &DeploymentInstance, starter: &Starter) -> Option<Self> {
        (!instance.env_from_launch.sets().is_empty()).then(|| Self {
            variables: instance.env_from_launch.clone(),
            instance: shown.to_owned(),
            starter: starter.clone(),
        })
    }
}

/// The instance that takes a set and what can start it, as a clause:
/// "`brain_inst` takes the first complete set from the launch, and the
/// launch starts it".
impl std::fmt::Display for ChoiceNeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_need(f, &self.instance, &self.starter, "the first complete set")
    }
}

/// The clause of a need: `instance` takes `taken` from the launch, and what
/// can start it.
fn write_need(
    f: &mut std::fmt::Formatter<'_>,
    instance: &str,
    starter: &Starter,
    taken: &str,
) -> std::fmt::Result {
    match starter {
        Starter::Launch => write!(
            f,
            "`{instance}` takes {taken} from the launch, and the launch starts it"
        ),
        Starter::Join => write!(
            f,
            "`{instance}` takes {taken} from the launch, and the join starts it"
        ),
        Starter::ScopedJoin {
            option,
            features,
            scopes,
        } => {
            let with = match features.as_slice() {
                [] => String::new(),
                [feature] => format!(" with the feature `{feature}`"),
                _ => format!(
                    " with the features {}",
                    crate::error::format_quoted_list(features)
                ),
            };
            write!(
                f,
                "`{instance}` of option `{option}` takes {taken} from the launch, and stack.join \
                 through {} can add a copy of `{option}`{with}",
                quoted_series(scopes, "or")
            )
        }
    }
}

/// `items` quoted and joined as a sentence lists them: `a`, `a` and `b`,
/// `a`, `b` and `c`, with `word` before the last one.
fn quoted_series<T: std::fmt::Display>(items: &[T], word: &str) -> String {
    match items {
        [] => String::new(),
        [one] => format!("`{one}`"),
        [rest @ .., last] => format!("{} {word} `{last}`", crate::error::format_quoted_list(rest)),
    }
}

/// The variables of a set as a sentence lists them: `A`, `B` and `C`.
fn quoted_set(set: &BTreeSet<LaunchVariable>) -> String {
    quoted_series(&set.iter().collect::<Vec<_>>(), "and")
}

/// The instances that take launch variables, each with what can start it:
/// one entry per variable of a list ([`VariableNeed`]), and one per
/// `any_of` ([`ChoiceNeed`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VariableNeeds {
    variables: Vec<VariableNeed>,
    choices: Vec<ChoiceNeed>,
}

/// What [`add_need`] reads of a need: whether two needs take the same
/// variables for the same instance, and what can start the instance.
trait Need: PartialEq {
    fn takes_as(&self, other: &Self) -> bool;
    fn starter(&self) -> &Starter;
    fn starter_mut(&mut self) -> &mut Starter;
}

impl Need for VariableNeed {
    fn takes_as(&self, other: &Self) -> bool {
        self.variable == other.variable && self.instance == other.instance
    }

    fn starter(&self) -> &Starter {
        &self.starter
    }

    fn starter_mut(&mut self) -> &mut Starter {
        &mut self.starter
    }
}

impl Need for ChoiceNeed {
    fn takes_as(&self, other: &Self) -> bool {
        self.variables == other.variables && self.instance == other.instance
    }

    fn starter(&self) -> &Starter {
        &self.starter
    }

    fn starter_mut(&mut self) -> &mut Starter {
        &mut self.starter
    }
}

/// Adds `need` to `held`. A need that an entry already gives is left out:
/// an entry that takes the same variables for the same instance and option,
/// through a scope of each scope of `need`, with features that `need` holds.
/// An entry that differs from `need` by its scopes alone takes the scopes of
/// `need`.
fn add_need<N: Need>(held: &mut Vec<N>, need: N) {
    let Starter::ScopedJoin {
        option,
        features,
        scopes,
    } = need.starter()
    else {
        if !held.contains(&need) {
            held.push(need);
        }
        return;
    };
    for entry in held.iter_mut() {
        if !entry.takes_as(&need) {
            continue;
        }
        let Starter::ScopedJoin {
            option: held_option,
            features: held_features,
            scopes: held_scopes,
        } = entry.starter_mut()
        else {
            continue;
        };
        if held_option != option {
            continue;
        }
        if held_features == features {
            for scope in scopes {
                if !held_scopes.contains(scope) {
                    held_scopes.push(scope.clone());
                }
            }
            return;
        }
        let held_covers = held_features
            .iter()
            .all(|feature| features.contains(feature))
            && scopes.iter().all(|scope| held_scopes.contains(scope));
        if held_covers {
            return;
        }
    }
    held.push(need);
}

impl VariableNeeds {
    /// Adds `need`, as [`add_need`] says.
    pub fn add(&mut self, need: VariableNeed) {
        add_need(&mut self.variables, need);
    }

    /// Adds the need of an `any_of`, as [`add_need`] says.
    pub fn add_choice(&mut self, need: ChoiceNeed) {
        add_need(&mut self.choices, need);
    }

    /// Adds the needs of `instance`, named `shown`, started by `starter`: one
    /// for each variable of its list, or one for its `any_of`.
    pub fn add_instance(&mut self, shown: &str, instance: &DeploymentInstance, starter: &Starter) {
        self.extend(VariableNeed::each_of(shown, instance, starter));
        if let Some(choice) = ChoiceNeed::of(shown, instance, starter) {
            self.add_choice(choice);
        }
    }

    /// Adds each need of `other`.
    pub fn extend(&mut self, other: impl IntoIterator<Item = VariableNeed>) {
        for need in other {
            self.add(need);
        }
    }

    /// Adds every need of `other`, of a variable and of an `any_of`.
    pub fn merge(&mut self, other: Self) {
        self.extend(other.variables);
        for choice in other.choices {
            self.add_choice(choice);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.variables.is_empty() && self.choices.is_empty()
    }

    /// The needs of the variables of lists.
    pub fn iter(&self) -> impl Iterator<Item = &VariableNeed> {
        self.variables.iter()
    }

    /// The needs of `any_of`s.
    pub fn choices(&self) -> impl Iterator<Item = &ChoiceNeed> {
        self.choices.iter()
    }

    /// Each variable of the lists of these needs, in name order, with each
    /// instance that takes it and what can start that instance, as one
    /// clause.
    pub fn reasons(&self) -> BTreeMap<LaunchVariable, String> {
        let mut grouped: BTreeMap<LaunchVariable, Vec<String>> = BTreeMap::new();
        for need in &self.variables {
            grouped
                .entry(need.variable.clone())
                .or_default()
                .push(need.to_string());
        }
        grouped
            .into_iter()
            .map(|(variable, needs)| (variable, needs.join("; ")))
            .collect()
    }

    /// Each `any_of` of these needs, in the order it is first added, with
    /// each instance that takes a set of it and what can start that
    /// instance, as one clause.
    pub fn choice_reasons(&self) -> Vec<(LaunchVariables, String)> {
        let mut grouped: Vec<(LaunchVariables, Vec<String>)> = Vec::new();
        for need in &self.choices {
            match grouped
                .iter_mut()
                .find(|(variables, _)| *variables == need.variables)
            {
                Some((_, needs)) => needs.push(need.to_string()),
                None => grouped.push((need.variables.clone(), vec![need.to_string()])),
            }
        }
        grouped
            .into_iter()
            .map(|(variables, needs)| (variables, needs.join("; ")))
            .collect()
    }

    /// The refusal of the change of `values` when a variable of a list of
    /// these needs is one that `values` do not hold, when neither source of
    /// `values` holds a set of an `any_of` complete, or when the own values
    /// of the change hold a set of an `any_of` in part: it names each such
    /// variable and `any_of`, each instance that needs it and what can start
    /// that instance.
    pub fn check(&self, values: ChangeValues<'_>) -> Result<(), MissingLaunchVariables> {
        let missing: BTreeMap<LaunchVariable, String> = self
            .reasons()
            .into_iter()
            .filter(|(variable, _)| !values.holds(variable.as_str()))
            .collect();
        let unmet: Vec<UnmetChoice> = self
            .choice_reasons()
            .into_iter()
            .filter_map(|(variables, reasons)| UnmetChoice::of(variables, reasons, values))
            .collect();
        if missing.is_empty() && unmet.is_empty() {
            Ok(())
        } else {
            Err(MissingLaunchVariables {
                change: values.change,
                missing,
                unmet,
            })
        }
    }
}

/// The change that needs launch variables, as a refusal names it: "the
/// launch" or "the join".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariableChange {
    Launch,
    Join,
}

impl VariableChange {
    /// The forwarded environment of the change, as a refusal names it.
    fn environment(self) -> &'static str {
        match self {
            Self::Launch => "the launch environment",
            Self::Join => "the join environment",
        }
    }

    /// The shell whose environment the CLI forwards with the change.
    fn own_shell(self) -> &'static str {
        match self {
            Self::Launch => "the shell that launches",
            Self::Join => "the shell that joins",
        }
    }
}

impl std::fmt::Display for VariableChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Launch => "the launch",
            Self::Join => "the join",
        })
    }
}

/// The values of the launch variables that a change gives to the instances
/// that it starts: its own, from its forwarded environment, and the values
/// that the launch kept. A join reads its own values first, then the kept
/// values; the values that a launch keeps are its own.
#[derive(Debug, Clone, Copy)]
pub struct ChangeValues<'a> {
    change: VariableChange,
    own: &'a LaunchValues,
    kept: &'a LaunchValues,
}

impl<'a> ChangeValues<'a> {
    /// The values of a launch: `own`, which the launch keeps.
    pub fn launch(own: &'a LaunchValues) -> Self {
        Self {
            change: VariableChange::Launch,
            own,
            kept: own,
        }
    }

    /// The values of a join: `own`, then `kept`, the values that the launch
    /// kept.
    pub fn join(own: &'a LaunchValues, kept: &'a LaunchValues) -> Self {
        Self {
            change: VariableChange::Join,
            own,
            kept,
        }
    }

    /// Whether the own values or the kept values hold the variable `name`.
    fn holds(&self, name: &str) -> bool {
        self.own.holds(name) || self.kept.holds(name)
    }

    /// Whether the own values or the kept values hold a set of `variables`
    /// complete, each source alone.
    fn hold_a_set_complete(&self, variables: &LaunchVariables) -> bool {
        variables.first_set_complete_in(self.own).is_some()
            || variables.first_set_complete_in(self.kept).is_some()
    }
}

/// A set of an `any_of` that an environment holds in part: some of its
/// variables, and not all.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SetHeldInPart {
    /// The variables of the set, in name order.
    set: BTreeSet<LaunchVariable>,
    /// The variables of the set that the environment does not hold, in name
    /// order.
    missing: Vec<LaunchVariable>,
}

impl SetHeldInPart {
    /// `set` when `values` hold some of its variables, and not all.
    fn of(set: &BTreeSet<LaunchVariable>, values: &LaunchValues) -> Option<Self> {
        let missing: Vec<LaunchVariable> = set
            .iter()
            .filter(|variable| !values.holds(variable.as_str()))
            .cloned()
            .collect();
        let held_in_part = !missing.is_empty() && missing.len() < set.len();
        held_in_part.then(|| Self {
            set: set.clone(),
            missing,
        })
    }
}

/// The variables of `held_in_part` that the environment does not hold, as
/// a sentence lists them, and the pronoun of these variables.
fn missing_of(held_in_part: &[SetHeldInPart]) -> (String, &'static str) {
    let missing: Vec<&LaunchVariable> =
        held_in_part.iter().flat_map(|held| &held.missing).collect();
    let them = if missing.len() == 1 { "it" } else { "them" };
    (quoted_series(&missing, "and"), them)
}

/// The sets of `held_in_part` as a sentence lists them: "the set of `A` and
/// `B` and the set of `C` and `D`".
fn sets_of(held_in_part: &[SetHeldInPart]) -> String {
    held_in_part
        .iter()
        .map(|held| format!("the set of {}", quoted_set(&held.set)))
        .collect::<Vec<_>>()
        .join(" and ")
}

/// What `environment` holds of the sets of `held_in_part`: "the launch
/// environment holds the set of `A` and `B` in part, as it does not hold
/// `B`, or holds it empty or with characters that a node cannot receive".
fn holds_in_part(environment: &str, held_in_part: &[SetHeldInPart]) -> String {
    let sets = sets_of(held_in_part);
    let (missing, them) = missing_of(held_in_part);
    format!(
        "{environment} holds {sets} in part, as it does not hold {missing}, or holds {them} \
         empty or with characters that a node cannot receive"
    )
}

/// Why a launch and a join refuse a set held in part.
const SET_HELD_IN_PART_RULE: &str = "A launch and a join refuse a set that their own environment \
                                     holds in part, also when another set is complete, so that \
                                     an instance never takes another set in its place";

/// What to do about a set held in part in the environment of `change`.
fn complete_or_empty(change: VariableChange) -> String {
    format!(
        "export every variable of such a set, or none of them, in {}",
        change.own_shell()
    )
}

/// An `any_of` that a change refuses: neither source of the values of the
/// change holds a set of it complete, or the own values of the change hold
/// a set of it in part.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UnmetChoice {
    variables: LaunchVariables,
    /// Whether a source of the values holds a set of the `any_of` complete.
    holds_a_set_complete: bool,
    /// The sets of the `any_of` that the own values of the change hold in
    /// part, in order.
    held_in_part: Vec<SetHeldInPart>,
    /// What needs the `any_of` ([`VariableNeeds::choice_reasons`]).
    reasons: String,
}

impl UnmetChoice {
    /// The refusal of `variables`, an `any_of` that `reasons` needs, when
    /// neither source of `values` holds a set of it complete, or when the
    /// own values hold a set of it in part; none when a source holds a set
    /// complete and the own values hold no set in part.
    fn of(variables: LaunchVariables, reasons: String, values: ChangeValues<'_>) -> Option<Self> {
        let holds_a_set_complete = values.hold_a_set_complete(&variables);
        let held_in_part = variables.sets_held_in_part(values.own);
        if holds_a_set_complete && held_in_part.is_empty() {
            return None;
        }
        Some(Self {
            variables,
            holds_a_set_complete,
            held_in_part,
            reasons,
        })
    }
}

/// A launch or a join refused because one of its instances takes a launch
/// variable that its environment does not hold, or an `any_of` none of whose
/// sets its environment holds complete or one of whose sets it holds in
/// part. The CLI does not forward a value with characters that a node cannot
/// receive, and the daemon counts an empty value as no value, so the refusal
/// also names these causes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingLaunchVariables {
    change: VariableChange,
    /// Each missing variable of a list, with what needs it
    /// ([`VariableNeeds::reasons`]).
    missing: BTreeMap<LaunchVariable, String>,
    /// Each `any_of` that the change refuses, with what needs it.
    unmet: Vec<UnmetChoice>,
}

impl MissingLaunchVariables {
    /// The variables of lists the change needs and does not get, in name
    /// order.
    pub fn variables(&self) -> impl Iterator<Item = &LaunchVariable> {
        self.missing.keys()
    }

    /// The shell where a value of the change is exported.
    fn shell(&self) -> &'static str {
        match self.change {
            VariableChange::Launch => "the shell that launches",
            VariableChange::Join => "the shell that joins or in the shell that launches",
        }
    }

    /// What to export for the `any_of`s and the missing variables of lists
    /// of the refusal: each missing variable, every variable of one set of
    /// each `any_of` with no complete set, and for a set held in part, every
    /// variable of it or none of them, in the shell of the change.
    fn remedy(&self) -> String {
        let with_no_complete_set = self
            .unmet
            .iter()
            .filter(|unmet| !unmet.holds_a_set_complete)
            .count();
        let of_sets = match with_no_complete_set {
            0 => None,
            1 => Some("every variable of one set"),
            _ => Some("every variable of one set of each"),
        };
        let shell = self.shell();
        let export = match (self.missing.is_empty(), of_sets) {
            (true, None) => String::new(),
            (false, None) => format!("Export each variable in {shell}. "),
            (true, Some(of_sets)) => format!("Export {of_sets} in {shell}. "),
            (false, Some(of_sets)) => format!("Export each variable, and {of_sets}, in {shell}. "),
        };
        let holds_a_set_in_part = self
            .unmet
            .iter()
            .any(|unmet| !unmet.held_in_part.is_empty());
        if !holds_a_set_in_part {
            return export;
        }
        format!(
            "{export}{SET_HELD_IN_PART_RULE}: {}. ",
            complete_or_empty(self.change)
        )
    }

    /// The refusal of one `any_of` and no missing variable of a list.
    fn write_one_choice(
        &self,
        f: &mut std::fmt::Formatter<'_>,
        unmet: &UnmetChoice,
    ) -> std::fmt::Result {
        let environment = if unmet.held_in_part.is_empty() {
            match self.change {
                VariableChange::Launch => {
                    "the launch environment holds no set complete, or holds a variable of it \
                     empty or with characters that a node cannot receive"
                        .to_owned()
                }
                VariableChange::Join => {
                    "neither the join environment nor the launch environment holds a set \
                     complete, or they hold a variable of it empty or with characters that a \
                     node cannot receive"
                        .to_owned()
                }
            }
        } else {
            holds_in_part(self.change.environment(), &unmet.held_in_part)
        };
        write!(
            f,
            "{} needs {}, and {environment}: {}. {}A value must not be empty, and must hold \
             {SAFE_ENV_VALUE_CHARACTERS} only",
            self.change,
            unmet.variables.describe(),
            unmet.reasons,
            self.remedy(),
        )
    }

    /// The refusal of several `any_of`s, or of `any_of`s and missing
    /// variables of lists: one line each.
    fn write_several(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let environment = match self.change {
            VariableChange::Launch => {
                "the launch environment does not hold, or holds empty or with characters that a \
                 node cannot receive"
            }
            VariableChange::Join => {
                "neither the join environment nor the launch environment holds, or they hold \
                 empty or with characters that a node cannot receive"
            }
        };
        let lines = self
            .missing
            .iter()
            .map(|(variable, reasons)| format!("`{variable}`: {reasons}"))
            .chain(self.unmet.iter().map(|unmet| {
                let in_part = if unmet.held_in_part.is_empty() {
                    String::new()
                } else {
                    let (missing, _) = missing_of(&unmet.held_in_part);
                    format!(
                        ", with {} held in part in {}, without {missing}",
                        sets_of(&unmet.held_in_part),
                        self.change.environment()
                    )
                };
                format!("{}{in_part}: {}", unmet.variables.describe(), unmet.reasons)
            }));
        write!(
            f,
            "{} needs variables that {environment}:{}.\n{}A value must not be empty, and must \
             hold {SAFE_ENV_VALUE_CHARACTERS} only",
            self.change,
            crate::error::format_bulleted(lines),
            self.remedy(),
        )
    }

    /// The refusal of missing variables of lists alone.
    fn write_missing(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let variables: Vec<&LaunchVariable> = self.missing.keys().collect();
        let one = variables.len() == 1;
        let them = if one { "it" } else { "them" };
        let environment = match self.change {
            VariableChange::Launch => {
                format!("the launch environment does not hold {them}, or holds {them}")
            }
            VariableChange::Join => format!(
                "neither the join environment nor the launch environment holds {them}, or they \
                 hold {them}"
            ),
        };
        let shell = self.shell();
        let (details, export) = if one {
            (
                format!(" {}.", self.missing[variables[0]]),
                format!(" Export `{}`", variables[0]),
            )
        } else {
            (
                format!(
                    "{}.",
                    crate::error::format_bulleted(
                        self.missing
                            .iter()
                            .map(|(variable, reasons)| format!("`{variable}`: {reasons}")),
                    )
                ),
                String::from("\nExport each of them"),
            )
        };
        write!(
            f,
            "{} needs {}, and {environment} empty or with characters that a node cannot \
             receive:{details}{export} in {shell}. A value must not be empty, and must hold \
             {SAFE_ENV_VALUE_CHARACTERS} only",
            self.change,
            quoted_series(&variables, "and"),
        )
    }
}

impl std::fmt::Display for MissingLaunchVariables {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.missing.is_empty(), self.unmet.as_slice()) {
            (_, []) => self.write_missing(f),
            (true, [unmet]) => self.write_one_choice(f, unmet),
            _ => self.write_several(f),
        }
    }
}

impl std::error::Error for MissingLaunchVariables {}

/// The refusal of a change that places an instance with launch variables on
/// a peer: a value of the launching shell stays on the machine where the
/// user typed it, the coordinator's. `None` when no such instance runs off
/// the coordinator.
pub fn refuse_peer_placements<'a>(
    instances: impl IntoIterator<Item = &'a DeploymentInstance>,
    placements: &Placements,
    change: VariableChange,
) -> Option<String> {
    let coordinator = placements.coordinator();
    let refusals: Vec<String> = instances
        .into_iter()
        .filter(|instance| !instance.env_from_launch.is_empty())
        .filter_map(|instance| {
            let id = instance.instance_id.as_str();
            let machine = placements.of(id);
            (machine != coordinator).then(|| {
                format!(
                    "`{id}` takes {} from the launch, and {change} places it on `{machine}`",
                    instance.env_from_launch.describe()
                )
            })
        })
        .collect();
    if refusals.is_empty() {
        return None;
    }
    Some(format!(
        "an instance with launch variables runs on the coordinator `{coordinator}` only, so a \
         value of the launching shell stays on the machine where it was typed; run each of these \
         on the coordinator:{}",
        crate::error::format_bulleted(refusals)
    ))
}

/// The key of a launcher that names launch variables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchVariablesKey {
    /// The `env_from_launch` of an instance entry.
    EnvFromLaunch,
    /// The `set_env_from_launch` of an adjustment.
    SetEnvFromLaunch,
}

/// One `env_from_launch` or `set_env_from_launch` of a launcher or of one of
/// its fragments, selected or not: its variables, the instance that they are
/// for, and where they are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrittenLaunchVariables<'a> {
    pub variables: &'a LaunchVariables,
    /// The id of the instance entry, or the target of the adjustment.
    pub instance: &'a str,
    pub key: LaunchVariablesKey,
    /// The label of the launcher or of the fragment that writes them.
    pub document: &'a str,
}

/// Where the variables are written: "the `env_from_launch` of `tool_inst`
/// in fleet.json5".
impl std::fmt::Display for WrittenLaunchVariables<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let key = match self.key {
            LaunchVariablesKey::EnvFromLaunch => "env_from_launch",
            LaunchVariablesKey::SetEnvFromLaunch => "set_env_from_launch",
        };
        write!(f, "the `{key}` of `{}` in {}", self.instance, self.document)
    }
}

/// A variable of a set of two variables or more of an `any_of` that another
/// `env_from_launch` or `set_env_from_launch` of the launcher names in a
/// list, or in a set that is not the same set. One forwarded environment
/// serves every instance of a launch, and the kept values serve each join:
/// an environment that holds the variable for the other one holds the set in
/// part, which a launch and a join refuse, or complete, and then the
/// instance of the set takes the value meant for the other one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableOutsideItsSet {
    pub variable: LaunchVariable,
    /// The set of two variables or more.
    pub set: BTreeSet<LaunchVariable>,
    /// Where the `any_of` of the set is written.
    pub set_written_by: String,
    /// The other one that names the variable: "a list", or "the set of `A`
    /// of an `any_of`".
    pub other: String,
    /// Where the other one is written.
    pub other_written_by: String,
}

impl std::fmt::Display for VariableOutsideItsSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            variable,
            set,
            set_written_by,
            other,
            other_written_by,
        } = self;
        write!(
            f,
            "`{variable}` is in the set of {} of an `any_of` ({set_written_by}) and in {other} \
             ({other_written_by}): an environment that holds `{variable}` for the second holds \
             the set in part, which a launch and a join refuse, or complete, and then the \
             instance of the set takes the value of the second; write a variable of a set of \
             two variables or more in that set only, or in the same set of another `any_of`",
            quoted_set(set)
        )
    }
}

impl std::error::Error for VariableOutsideItsSet {}

/// The other one of `written` that names a variable of `set`, a set of two
/// variables or more, outside a set equal to it: a list, or another set.
fn named_outside(
    set: &BTreeSet<LaunchVariable>,
    written: &WrittenLaunchVariables<'_>,
) -> Option<(LaunchVariable, String)> {
    let shared = |other: &BTreeSet<LaunchVariable>| other.intersection(set).next().cloned();
    let Form::AnyOf(sets) = &written.variables.0 else {
        let list: BTreeSet<LaunchVariable> = written.variables.required().cloned().collect();
        return shared(&list).map(|variable| (variable, "a list".to_owned()));
    };
    sets.iter().filter(|other| *other != set).find_map(|other| {
        shared(other).map(|variable| {
            (
                variable,
                format!("the set of {} of an `any_of`", quoted_set(other)),
            )
        })
    })
}

/// The refusal of a launcher, `written` its every `env_from_launch` and
/// `set_env_from_launch`, when one of them names a variable of a set of two
/// variables or more of an `any_of` of another one in a list, or in a set
/// that is not the same set. Two `any_of`s can share a whole set. The first
/// such variable in the order of `written` is named.
pub fn refuse_variables_outside_their_set(
    written: &[WrittenLaunchVariables<'_>],
) -> Result<(), VariableOutsideItsSet> {
    for (index, holder) in written.iter().enumerate() {
        for set in holder.variables.sets().iter().filter(|set| set.len() >= 2) {
            let outside = written
                .iter()
                .enumerate()
                .filter(|(other_index, _)| *other_index != index)
                .find_map(|(_, other)| named_outside(set, other).map(|found| (found, other)));
            let Some(((variable, other), other_written)) = outside else {
                continue;
            };
            return Err(VariableOutsideItsSet {
                variable,
                set: set.clone(),
                set_written_by: holder.to_string(),
                other,
                other_written_by: other_written.to_string(),
            });
        }
    }
    Ok(())
}

/// One set of an `any_of` of a launcher that the launch environment holds in
/// part, with the first `env_from_launch` or `set_env_from_launch` that names
/// its `any_of`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct KeptSetHeldInPart {
    held: SetHeldInPart,
    any_of: String,
    written_by: String,
}

/// The refusal of a launch whose environment holds in part a set of an
/// `any_of` of its launcher. The launch keeps the value of each launch
/// variable of its launcher for each later join, so it refuses the set also
/// when no instance that it can start takes that `any_of`: the kept values
/// never hold a set in part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptSetsHeldInPart(Vec<KeptSetHeldInPart>);

/// The refusal of a launch whose values are `launch`, `written` every
/// `env_from_launch` and `set_env_from_launch` of its launcher, when
/// `launch` holds a set of one of their `any_of`s in part. Each set is named
/// once, with the first one of `written` that names it.
pub fn refuse_kept_sets_held_in_part(
    written: &[WrittenLaunchVariables<'_>],
    launch: &LaunchValues,
) -> Result<(), KeptSetsHeldInPart> {
    let mut found: Vec<KeptSetHeldInPart> = Vec::new();
    for holder in written {
        for held in holder.variables.sets_held_in_part(launch) {
            if found.iter().any(|kept| kept.held.set == held.set) {
                continue;
            }
            found.push(KeptSetHeldInPart {
                held,
                any_of: holder.variables.describe(),
                written_by: holder.to_string(),
            });
        }
    }
    if found.is_empty() {
        Ok(())
    } else {
        Err(KeptSetsHeldInPart(found))
    }
}

impl std::fmt::Display for KeptSetsHeldInPart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names =
            |kept: &KeptSetHeldInPart| format!("{} names it in {}", kept.written_by, kept.any_of);
        match self.0.as_slice() {
            [one] => write!(
                f,
                "{}: {}, and the launch keeps the value of each launch variable of its launcher \
                 for the joins. ",
                holds_in_part(
                    VariableChange::Launch.environment(),
                    std::slice::from_ref(&one.held)
                ),
                names(one)
            )?,
            several => write!(
                f,
                "the launch environment holds these sets in part, or holds a variable of them \
                 empty or with characters that a node cannot receive:{}.\nThe launch keeps the \
                 value of each launch variable of its launcher for the joins. ",
                crate::error::format_bulleted(several.iter().map(|kept| {
                    let (missing, _) = missing_of(std::slice::from_ref(&kept.held));
                    format!(
                        "the set of {}, without {missing}: {}",
                        quoted_set(&kept.held.set),
                        names(kept)
                    )
                }))
            )?,
        }
        write!(
            f,
            "{SET_HELD_IN_PART_RULE}: {}. A value must not be empty, and must hold \
             {SAFE_ENV_VALUE_CHARACTERS} only",
            complete_or_empty(VariableChange::Launch)
        )
    }
}

impl std::error::Error for KeptSetsHeldInPart {}

/// The value of each launch variable that a forwarded environment holds, not
/// empty. The daemon keeps those of the launch, in memory, for the life of
/// the stack. The debug output names the variables only.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct LaunchValues(BTreeMap<LaunchVariable, String>);

impl LaunchValues {
    /// The value of the variable `name`.
    pub fn value_of(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// Whether these values hold the variable `name`.
    pub fn holds(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }
}

impl std::fmt::Debug for LaunchValues {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("LaunchValues")
            .field(
                &self
                    .0
                    .keys()
                    .map(LaunchVariable::as_str)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// The environment that the CLI forwarded with a launch or a join, parsed
/// against the launch variables of the launcher of the stack. The value of a
/// launch variable is kept apart ([`Self::launch_values`]): it reaches the
/// run of each instance that names it, and nothing else. Each other entry
/// ([`Self::common`]) reaches every node that the change adds, builds and
/// starts on this machine. An empty value of a launch variable counts as no
/// value: a shell that sources a `.env` template with an unset key exports
/// one, and a node cannot use it. The debug output names the variables only.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ForwardedEnv {
    common: Vec<(String, String)>,
    launch_values: LaunchValues,
}

impl ForwardedEnv {
    /// Parses `entries`, the forwarded environment in the order it arrived,
    /// against `launch_variables`, every launch variable of the launcher.
    /// The CLI forwards each key once; when a goal gives a launch variable
    /// two times, the later value wins, as it does in a process environment.
    pub fn parse(
        entries: Vec<(String, String)>,
        launch_variables: &BTreeSet<LaunchVariable>,
    ) -> Self {
        let mut common = Vec::new();
        let mut launch_values = BTreeMap::new();
        for (key, value) in entries {
            let Some(variable) = launch_variables.get(key.as_str()) else {
                common.push((key, value));
                continue;
            };
            if value.is_empty() {
                launch_values.remove(variable);
            } else {
                launch_values.insert(variable.clone(), value);
            }
        }
        Self {
            common,
            launch_values: LaunchValues(launch_values),
        }
    }

    /// The entries that are not launch variables, in the order they arrived.
    pub fn common(&self) -> &[(String, String)] {
        &self.common
    }

    /// The value of each launch variable that the environment holds, not
    /// empty.
    pub fn launch_values(&self) -> &LaunchValues {
        &self.launch_values
    }
}

impl std::fmt::Debug for ForwardedEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForwardedEnv")
            .field(
                "common",
                &self
                    .common
                    .iter()
                    .map(|(key, _)| key.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("launch_values", &self.launch_values)
            .finish()
    }
}

/// The `any_of` of a hosted brain, which the tests of this crate and of the
/// crates that use it share: the base address and the key of an
/// OpenAI-compatible API, else the key of the provider.
#[cfg(any(test, feature = "test-support"))]
pub mod hosted_brain {
    use super::{DeploymentInstance, ForwardedEnv, LaunchValues, LaunchVariables};
    use config::runtime::Name;

    /// The base address of the API, in the first set.
    pub const BASE: &str = "OPENAI_COMPATIBLE_API_BASE";
    /// The key of the API, in the first set.
    pub const API_KEY: &str = "OPENAI_COMPATIBLE_API_KEY";
    /// The key of the provider, the second set.
    pub const PROVIDER_KEY: &str = "GEMINI_API_KEY";
    /// The `any_of` as a launcher writes it, the variables of the first set
    /// out of name order.
    pub const ANY_OF: &str = r#"{ any_of: [
        ["OPENAI_COMPATIBLE_API_KEY", "OPENAI_COMPATIBLE_API_BASE"],
        ["GEMINI_API_KEY"],
    ] }"#;

    /// The `any_of`, parsed.
    pub fn any_of() -> LaunchVariables {
        serde_json5::from_str(ANY_OF).expect("the any_of of the hosted brain parses")
    }

    /// The instance `id`, which takes the `any_of`.
    pub fn brain(id: &str) -> DeploymentInstance {
        let mut brain = DeploymentInstance::empty(Name::new(id).expect("an instance id"));
        brain.env_from_launch = any_of();
        brain
    }

    /// `entries` as the forwarded environment of a launcher whose launch
    /// variables are the names of the `any_of`.
    pub fn forwarded(entries: &[(&str, &str)]) -> ForwardedEnv {
        ForwardedEnv::parse(
            entries
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
            &any_of().iter().cloned().collect(),
        )
    }

    /// The launch values of `entries`, as [`forwarded`] parses them.
    pub fn values(entries: &[(&str, &str)]) -> LaunchValues {
        forwarded(entries).launch_values().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::runtime::CoreNodeName;

    fn variable(name: &str) -> LaunchVariable {
        LaunchVariable::try_from(name.to_owned()).expect("a launch variable")
    }

    fn name(value: &str) -> Name {
        Name::new(value).expect("a name")
    }

    /// Launch values that hold each variable of `names`.
    fn values(names: &[&str]) -> LaunchValues {
        LaunchValues(
            names
                .iter()
                .map(|name| (variable(name), "value".to_owned()))
                .collect(),
        )
    }

    fn entries(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn scoped(
        variable_name: &str,
        instance: &str,
        features: &[&str],
        scopes: &[&str],
    ) -> VariableNeed {
        VariableNeed {
            variable: variable(variable_name),
            instance: instance.to_owned(),
            starter: Starter::ScopedJoin {
                option: name("demo_arm"),
                features: features.iter().map(|feature| name(feature)).collect(),
                scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
            },
        }
    }

    /// A list parses each name by the rules of a launch variable, refuses a
    /// name written two times, and writes back in name order.
    #[test]
    fn a_list_parses_each_name_once() {
        let parsed: LaunchVariables =
            serde_json5::from_str(r#"["OPENROUTER_API_KEY", "GEMINI_API_KEY"]"#).unwrap();
        assert_eq!(
            parsed
                .iter()
                .map(LaunchVariable::as_str)
                .collect::<Vec<_>>(),
            ["GEMINI_API_KEY", "OPENROUTER_API_KEY"]
        );
        assert_eq!(
            serde_json5::to_string(&parsed).unwrap(),
            r#"["GEMINI_API_KEY","OPENROUTER_API_KEY"]"#
        );
        for (written, expected) in [
            (r#"["A", "B", "A"]"#, "names `A` two times"),
            (
                r#"["has-dash"]"#,
                "`has-dash` is not a valid shell identifier",
            ),
            (r#"["LD_PRELOAD"]"#, "`LD_PRELOAD` is reserved"),
            (r#"["TMPDIR"]"#, "`TMPDIR` is never forwarded by the CLI"),
        ] {
            let error = serde_json5::from_str::<LaunchVariables>(written)
                .expect_err(written)
                .to_string();
            assert!(error.contains(expected), "{written}: {error}");
        }
    }

    /// Two lists that name the same variables in another order are the same
    /// list.
    #[test]
    fn two_lists_of_the_same_variables_in_another_order_are_equal() {
        let ab: LaunchVariables = serde_json5::from_str(r#"["A_KEY", "B_KEY"]"#).unwrap();
        let ba: LaunchVariables = serde_json5::from_str(r#"["B_KEY", "A_KEY"]"#).unwrap();
        let a: LaunchVariables = serde_json5::from_str(r#"["A_KEY"]"#).unwrap();
        assert_eq!(ab, ba);
        assert_ne!(ab, a);
    }

    /// Each variable of the list that `env_vars` also gives is found, in
    /// name order.
    #[test]
    fn every_variable_that_env_vars_also_gives_is_found() {
        let list: LaunchVariables =
            serde_json5::from_str(r#"["C_KEY", "A_KEY", "B_KEY"]"#).unwrap();
        let env_vars = BTreeMap::from([
            ("C_KEY".to_owned(), "c".to_owned()),
            ("A_KEY".to_owned(), "a".to_owned()),
            ("OTHER".to_owned(), "o".to_owned()),
        ]);
        assert_eq!(
            list.given_in(&env_vars)
                .map(LaunchVariable::as_str)
                .collect::<Vec<_>>(),
            ["A_KEY", "C_KEY"]
        );
    }

    /// The refusal of one variable given two values is one sentence; the
    /// refusal of several lists each instance and variable.
    #[test]
    fn the_refusal_of_variables_given_two_values_names_each_one() {
        let one = VariableGivenTwoValues {
            instance: "brain_inst".to_owned(),
            variable: variable("GEMINI_API_KEY"),
            list_origin: Some("the adjustment in demo.json5".to_owned()),
        };
        let other = VariableGivenTwoValues {
            instance: "planner_inst".to_owned(),
            variable: variable("PLANNER_KEY"),
            list_origin: None,
        };
        VariablesGivenTwoValues::refuse(Vec::new()).unwrap();
        assert_eq!(
            VariablesGivenTwoValues::refuse(vec![one.clone()])
                .expect_err("one")
                .to_string(),
            "instance `brain_inst` takes `GEMINI_API_KEY` from the launch in `env_from_launch` \
             (set by the adjustment in demo.json5) and gives it a value in `env_vars`, so the \
             variable would get two values; take it out of one of the two"
        );
        assert_eq!(
            VariablesGivenTwoValues::refuse(vec![one, other])
                .expect_err("two")
                .to_string(),
            "these instances take a variable from the launch and give it a value in `env_vars`, \
             so each of these variables would get two values; take each one out of one of the \
             two:\n  - instance `brain_inst` takes `GEMINI_API_KEY` from the launch in \
             `env_from_launch` (set by the adjustment in demo.json5) and gives it a value in \
             `env_vars`\n  - instance `planner_inst` takes `PLANNER_KEY` from the launch in \
             `env_from_launch` and gives it a value in `env_vars`"
        );
    }

    /// A need through a scope that a smaller feature set of the same scope
    /// already gives is left out, and the same need through another scope
    /// adds that scope.
    #[test]
    fn a_scoped_need_keeps_its_smallest_feature_sets_and_every_scope() {
        let mut needs = VariableNeeds::default();
        needs.add(scoped("KEY", "brain_inst", &["router"], &["controls_inst"]));
        needs.add(scoped(
            "KEY",
            "brain_inst",
            &["router", "camera"],
            &["controls_inst"],
        ));
        needs.add(scoped("KEY", "brain_inst", &["router"], &["applier_inst"]));
        needs.add(scoped("KEY", "brain_inst", &["camera"], &["controls_inst"]));
        needs.add(scoped(
            "OTHER",
            "brain_inst",
            &["router", "camera"],
            &["controls_inst"],
        ));

        assert_eq!(
            needs.iter().cloned().collect::<Vec<_>>(),
            [
                scoped(
                    "KEY",
                    "brain_inst",
                    &["router"],
                    &["controls_inst", "applier_inst"]
                ),
                scoped("KEY", "brain_inst", &["camera"], &["controls_inst"]),
                scoped(
                    "OTHER",
                    "brain_inst",
                    &["router", "camera"],
                    &["controls_inst"]
                ),
            ]
        );
    }

    /// The refusal of one variable is one sentence: the variable, each
    /// instance that needs it with what can start it, and what to export.
    #[test]
    fn the_refusal_of_one_variable_names_each_instance_and_what_starts_it() {
        let mut needs = VariableNeeds::default();
        needs.add(scoped(
            "GEMINI_API_KEY",
            "brain_inst",
            &[],
            &["framework_controls_inst"],
        ));
        needs.add(VariableNeed {
            variable: variable("GEMINI_API_KEY"),
            instance: "planner_inst".to_owned(),
            starter: Starter::Launch,
        });

        let refusal = needs
            .check(ChangeValues::launch(&values(&[])))
            .expect_err("nothing holds the key")
            .to_string();
        assert_eq!(
            refusal,
            "the launch needs `GEMINI_API_KEY`, and the launch environment does not hold it, or \
             holds it empty or with characters that a node cannot receive: `brain_inst` of \
             option `demo_arm` takes it from the launch, and stack.join through \
             `framework_controls_inst` can add a copy of `demo_arm`; `planner_inst` takes it \
             from the launch, and the launch starts it. Export `GEMINI_API_KEY` in the shell \
             that launches. A value must not be empty, and must hold letters, digits and \
             `_ - . / : , = @ + %` only"
        );
        needs
            .check(ChangeValues::launch(&values(&["GEMINI_API_KEY"])))
            .unwrap();
    }

    /// The refusal of several variables lists each one, with its own
    /// instances, and names the features and every scope of a scoped need.
    #[test]
    fn the_refusal_of_several_variables_lists_each_one() {
        let mut needs = VariableNeeds::default();
        needs.add(scoped(
            "B_KEY",
            "brain_inst",
            &["router"],
            &["a_inst", "b_inst"],
        ));
        needs.add(scoped(
            "A_KEY",
            "brain_inst",
            &["router", "camera"],
            &["a_inst"],
        ));
        needs.add(VariableNeed {
            variable: variable("C_KEY"),
            instance: "copy_brain_inst".to_owned(),
            starter: Starter::Join,
        });

        let refusal = needs
            .check(ChangeValues::join(&values(&["C_KEY"]), &values(&[])))
            .expect_err("two keys are missing");
        assert_eq!(
            refusal
                .variables()
                .map(LaunchVariable::as_str)
                .collect::<Vec<_>>(),
            ["A_KEY", "B_KEY"]
        );
        assert_eq!(
            refusal.to_string(),
            "the join needs `A_KEY` and `B_KEY`, and neither the join environment nor the launch \
             environment holds them, or they hold them empty or with characters that a node \
             cannot receive:\n  - `A_KEY`: `brain_inst` of option `demo_arm` takes it from the \
             launch, and stack.join through `a_inst` can add a copy of `demo_arm` with the \
             features `router`, `camera`\n  - `B_KEY`: `brain_inst` of option `demo_arm` takes \
             it from the launch, and stack.join through `a_inst` or `b_inst` can add a copy of \
             `demo_arm` with the feature `router`.\nExport each of them in the shell that joins \
             or in the shell that launches. A value must not be empty, and must hold letters, \
             digits and `_ - . / : , = @ + %` only"
        );
    }

    /// The reasons group the needs by variable, in name order.
    #[test]
    fn the_reasons_group_the_needs_by_variable() {
        let mut needs = VariableNeeds::default();
        needs.add(VariableNeed {
            variable: variable("B_KEY"),
            instance: "planner_inst".to_owned(),
            starter: Starter::Launch,
        });
        needs.add(scoped("A_KEY", "brain_inst", &[], &["controls_inst"]));
        needs.add(VariableNeed {
            variable: variable("B_KEY"),
            instance: "helper_inst".to_owned(),
            starter: Starter::Launch,
        });

        assert_eq!(
            needs.reasons(),
            BTreeMap::from([
                (
                    variable("A_KEY"),
                    "`brain_inst` of option `demo_arm` takes it from the launch, and stack.join \
                     through `controls_inst` can add a copy of `demo_arm`"
                        .to_owned()
                ),
                (
                    variable("B_KEY"),
                    "`planner_inst` takes it from the launch, and the launch starts it; \
                     `helper_inst` takes it from the launch, and the launch starts it"
                        .to_owned()
                ),
            ])
        );
    }

    /// An instance with launch variables on a peer is refused, naming the
    /// instance, its variables and the peer; one with none may run there.
    #[test]
    fn an_instance_with_launch_variables_is_refused_on_a_peer() {
        let mut brain = DeploymentInstance::empty(name("brain_inst"));
        brain.env_from_launch =
            LaunchVariables::try_from(vec![variable("GEMINI_API_KEY")]).unwrap();
        let mut camera = DeploymentInstance::empty(name("camera_inst"));
        camera.core_node = Some("rig".to_owned());
        let core_node = |value: &str| CoreNodeName::new(value).unwrap();
        let placements = Placements::new(
            core_node("coordinator"),
            BTreeMap::from([
                ("brain_inst".to_owned(), core_node("peer_b")),
                ("camera_inst".to_owned(), core_node("peer_b")),
            ]),
        );

        let refusal = refuse_peer_placements([&brain, &camera], &placements, VariableChange::Join)
            .expect("the brain runs on a peer");
        assert!(
            refusal.contains(
                "`brain_inst` takes `GEMINI_API_KEY` from the launch, and the join places it on \
                 `peer_b`"
            ),
            "{refusal}"
        );
        assert!(!refusal.contains("camera_inst"), "{refusal}");
        assert!(
            refuse_peer_placements(
                [&brain],
                &Placements::all_on(core_node("coordinator")),
                VariableChange::Launch
            )
            .is_none()
        );
    }

    /// The forwarded environment keeps apart the value of each launch
    /// variable, not empty, the later of two, and keeps every other entry in
    /// the order it arrived, an empty one included.
    #[test]
    fn the_forwarded_environment_keeps_the_launch_values_apart() {
        let names = BTreeSet::from([
            variable("GEMINI_API_KEY"),
            variable("EMPTY_KEY"),
            variable("TWICE_KEY"),
            variable("ABSENT_KEY"),
        ]);
        let parsed = ForwardedEnv::parse(
            entries(&[
                ("PATH", "/usr/bin"),
                ("GEMINI_API_KEY", "sentinel-value"),
                ("EMPTY_KEY", ""),
                ("TWICE_KEY", "first"),
                ("BLANK", ""),
                ("TWICE_KEY", "second"),
            ]),
            &names,
        );

        assert_eq!(
            parsed.common(),
            entries(&[("PATH", "/usr/bin"), ("BLANK", "")])
        );
        let values = parsed.launch_values();
        assert_eq!(values.value_of("GEMINI_API_KEY"), Some("sentinel-value"));
        assert_eq!(values.value_of("TWICE_KEY"), Some("second"));
        assert!(!values.holds("EMPTY_KEY"), "an empty value is no value");
        assert!(!values.holds("ABSENT_KEY"));
        assert!(
            !values.holds("PATH"),
            "only a launch variable is a launch value"
        );
    }

    /// A later empty value of a launch variable takes back the earlier one.
    #[test]
    fn a_later_empty_value_of_a_launch_variable_is_no_value() {
        let names = BTreeSet::from([variable("GEMINI_API_KEY")]);
        let parsed = ForwardedEnv::parse(
            entries(&[("GEMINI_API_KEY", "first"), ("GEMINI_API_KEY", "")]),
            &names,
        );
        assert!(!parsed.launch_values().holds("GEMINI_API_KEY"));
    }

    /// The debug output of the forwarded environment and of the launch
    /// values names the variables only.
    #[test]
    fn the_debug_output_names_the_variables_only() {
        let names = BTreeSet::from([variable("GEMINI_API_KEY")]);
        let parsed = ForwardedEnv::parse(
            entries(&[("GEMINI_API_KEY", "sentinel-value"), ("PATH", "/usr/bin")]),
            &names,
        );
        assert_eq!(
            format!("{parsed:?}"),
            r#"ForwardedEnv { common: ["PATH"], launch_values: LaunchValues(["GEMINI_API_KEY"]) }"#
        );
    }

    fn any_of(written: &str) -> LaunchVariables {
        serde_json5::from_str(written).expect(written)
    }

    fn names<'a>(variables: impl IntoIterator<Item = &'a LaunchVariable>) -> Vec<&'a str> {
        variables.into_iter().map(LaunchVariable::as_str).collect()
    }

    /// An `any_of` keeps its sets in the order written, each set in name
    /// order, and writes back as an object with the one key `any_of`. Every
    /// name of every set is a variable that the instance can take, and none
    /// is one that it takes each time.
    #[test]
    fn an_any_of_keeps_its_sets_in_order_and_writes_back_as_an_object() {
        let parsed = hosted_brain::any_of();
        assert_eq!(
            parsed.sets().iter().map(names).collect::<Vec<_>>(),
            [
                vec!["OPENAI_COMPATIBLE_API_BASE", "OPENAI_COMPATIBLE_API_KEY"],
                vec!["GEMINI_API_KEY"],
            ]
        );
        assert_eq!(
            serde_json5::to_string(&parsed).unwrap(),
            r#"{"any_of":[["OPENAI_COMPATIBLE_API_BASE","OPENAI_COMPATIBLE_API_KEY"],["GEMINI_API_KEY"]]}"#
        );
        assert_eq!(
            serde_json5::from_str::<LaunchVariables>(&serde_json5::to_string(&parsed).unwrap())
                .unwrap(),
            parsed
        );
        assert_eq!(
            names(parsed.iter()),
            [
                "GEMINI_API_KEY",
                "OPENAI_COMPATIBLE_API_BASE",
                "OPENAI_COMPATIBLE_API_KEY"
            ]
        );
        assert_eq!(parsed.required().count(), 0);
        assert!(!parsed.is_empty());
        let list: LaunchVariables = serde_json5::from_str(r#"["B_KEY", "A_KEY"]"#).unwrap();
        assert_eq!(names(list.required()), ["A_KEY", "B_KEY"]);
        assert!(list.sets().is_empty());
    }

    /// The same sets in another order are another `any_of`, and an `any_of`
    /// is never the list of its names.
    #[test]
    fn the_order_of_the_sets_is_part_of_an_any_of() {
        let first = any_of(r#"{ any_of: [["A_KEY"], ["B_KEY"]] }"#);
        let second = any_of(r#"{ any_of: [["B_KEY"], ["A_KEY"]] }"#);
        let list: LaunchVariables = serde_json5::from_str(r#"["A_KEY", "B_KEY"]"#).unwrap();
        assert_ne!(first, second);
        assert_ne!(first, list);
        assert_eq!(first, any_of(r#"{ any_of: [["A_KEY"], ["B_KEY"]] }"#));
    }

    /// The message of two sets that share a variable.
    fn shared(earlier: usize, later: usize, variable: &str) -> String {
        format!(
            "sets {earlier} and {later} of `any_of` both name `{variable}`, so set {later} is \
             never taken: when the environment holds it complete, it holds set {earlier} \
             complete too, or in part, which is refused; write each variable in one set only"
        )
    }

    /// Two sets that share a variable are refused, whatever their order and
    /// size: two equal sets, a set that holds an earlier one, a set that an
    /// earlier one holds, and two sets that only overlap. The message names
    /// the first earlier set and the first shared variable in name order.
    #[test]
    fn two_sets_that_share_a_variable_are_refused() {
        for (written, expected) in [
            (
                r#"{ any_of: [["A_KEY"], ["A_KEY"]] }"#,
                shared(1, 2, "A_KEY"),
            ),
            (
                r#"{ any_of: [["A_KEY", "B_KEY"], ["B_KEY", "A_KEY"]] }"#,
                shared(1, 2, "A_KEY"),
            ),
            (
                r#"{ any_of: [["A_KEY"], ["B_KEY", "A_KEY"]] }"#,
                shared(1, 2, "A_KEY"),
            ),
            (
                r#"{ any_of: [["A_KEY", "B_KEY"], ["B_KEY"]] }"#,
                shared(1, 2, "B_KEY"),
            ),
            (
                r#"{ any_of: [["A_KEY", "B_KEY"], ["C_KEY", "B_KEY"]] }"#,
                shared(1, 2, "B_KEY"),
            ),
            (
                r#"{ any_of: [["C_KEY"], ["A_KEY", "B_KEY"], ["D_KEY", "C_KEY", "B_KEY"]] }"#,
                shared(1, 3, "C_KEY"),
            ),
        ] {
            let error = serde_json5::from_str::<LaunchVariables>(written)
                .expect_err(written)
                .to_string();
            assert!(error.contains(&expected), "{written}: {error}");
        }
    }

    /// Each `any_of` that an instance cannot use is refused at the parse, and
    /// the message names the set by its place, from 1.
    #[test]
    fn a_broken_any_of_is_refused_naming_the_set() {
        for (written, expected) in [
            (
                r"{ any_of: [] }",
                "`any_of` names no set; an instance that takes no variable from the launch \
                 writes `[]`",
            ),
            (
                r#"{ any_of: [["A_KEY"]] }"#,
                "`any_of` names one set; write that set as a list",
            ),
            (
                r#"{ any_of: [["A_KEY"], []] }"#,
                "set 2 of `any_of` is empty; each set names at least one variable",
            ),
            (
                r#"{ any_of: [["A_KEY", "B_KEY", "A_KEY"], ["C_KEY"]] }"#,
                "set 1 of `any_of` names `A_KEY` two times; a set names each variable once",
            ),
            (
                r#"{ any_of: [["has-dash"], ["B_KEY"]] }"#,
                "`has-dash` is not a valid shell identifier",
            ),
            (
                r#"{ any_of: [["A_KEY"], ["B_KEY", "LD_PRELOAD"]] }"#,
                "`LD_PRELOAD` is reserved",
            ),
            (
                r#"{ any_of: [["TMPDIR"], ["B_KEY"]] }"#,
                "`TMPDIR` is never forwarded by the CLI",
            ),
            (
                r#"{ any_of: [["A_KEY"], ["B_KEY"]], all: ["C_KEY"] }"#,
                "unknown field `all`",
            ),
            (r"{}", "missing field `any_of`"),
            (r#""A_KEY""#, "a list of variable names, or an object"),
        ] {
            let error = serde_json5::from_str::<LaunchVariables>(written)
                .expect_err(written)
                .to_string();
            assert!(error.contains(expected), "{written}: {error}");
        }
    }

    /// The names of the variables that `variables` take from `own`, then
    /// `kept`.
    fn taken<'a>(
        variables: &'a LaunchVariables,
        own: &'a LaunchValues,
        kept: &'a LaunchValues,
    ) -> Vec<&'a str> {
        names(
            variables
                .values_taken(own, kept)
                .into_iter()
                .map(|(variable, _)| variable),
        )
    }

    /// The variables taken from one source: each one of a list that it
    /// holds, and of an `any_of` the first complete set alone, or none.
    #[test]
    fn the_first_complete_set_is_taken() {
        let brain = hosted_brain::any_of();
        let none = values(&[]);
        let api = ["OPENAI_COMPATIBLE_API_BASE", "OPENAI_COMPATIBLE_API_KEY"];
        for (held, expected) in [
            (
                &[
                    "OPENAI_COMPATIBLE_API_BASE",
                    "OPENAI_COMPATIBLE_API_KEY",
                    "GEMINI_API_KEY",
                ][..],
                &api[..],
            ),
            (
                &["OPENAI_COMPATIBLE_API_BASE", "OPENAI_COMPATIBLE_API_KEY"][..],
                &api[..],
            ),
            (
                &["OPENAI_COMPATIBLE_API_KEY", "GEMINI_API_KEY"][..],
                &["GEMINI_API_KEY"][..],
            ),
            (&["GEMINI_API_KEY"][..], &["GEMINI_API_KEY"][..]),
            (&["OPENAI_COMPATIBLE_API_BASE"][..], &[][..]),
            (&[][..], &[][..]),
        ] {
            assert_eq!(taken(&brain, &values(held), &none), expected, "{held:?}");
            assert_eq!(taken(&brain, &none, &values(held)), expected, "{held:?}");
        }
        let list: LaunchVariables = serde_json5::from_str(r#"["B_KEY", "A_KEY"]"#).unwrap();
        assert_eq!(
            taken(&list, &values(&["A_KEY", "B_KEY", "C_KEY"]), &none),
            ["A_KEY", "B_KEY"]
        );
        assert_eq!(taken(&list, &values(&["B_KEY"]), &none), ["B_KEY"]);
    }

    /// Launch values that hold `entries`, each variable with its value.
    fn with_values(entries: &[(&str, &str)]) -> LaunchValues {
        LaunchValues(
            entries
                .iter()
                .map(|(name, value)| (variable(name), (*value).to_owned()))
                .collect(),
        )
    }

    /// A variable of a list takes the own value of the change, else the kept
    /// one. An `any_of` takes the first set that the own values hold
    /// complete, also before an earlier set of the kept values, else the
    /// first set that the kept values hold complete, and every value of the
    /// set comes from that one source.
    #[test]
    fn the_own_values_come_first_and_a_set_takes_its_values_from_one_source() {
        let list: LaunchVariables = serde_json5::from_str(r#"["A_KEY", "B_KEY"]"#).unwrap();
        assert_eq!(
            list.values_taken(
                &with_values(&[("A_KEY", "own")]),
                &with_values(&[("A_KEY", "kept"), ("B_KEY", "kept")])
            ),
            [(&variable("A_KEY"), "own"), (&variable("B_KEY"), "kept")]
        );

        let brain = hosted_brain::any_of();
        let base = variable(hosted_brain::BASE);
        let api_key = variable(hosted_brain::API_KEY);
        let provider_key = variable(hosted_brain::PROVIDER_KEY);
        let kept_api = with_values(&[
            (hosted_brain::BASE, "kept-base"),
            (hosted_brain::API_KEY, "kept-key"),
        ]);
        assert_eq!(
            brain.values_taken(
                &with_values(&[(hosted_brain::PROVIDER_KEY, "own-provider-key")]),
                &kept_api
            ),
            [(&provider_key, "own-provider-key")],
            "a complete set of the own values comes before an earlier set of the kept values"
        );
        assert_eq!(
            brain.values_taken(
                &with_values(&[(hosted_brain::API_KEY, "own-key")]),
                &kept_api
            ),
            [(&base, "kept-base"), (&api_key, "kept-key")],
            "an own value adds nothing to a set of the kept values"
        );
        assert!(
            brain
                .values_taken(
                    &with_values(&[(hosted_brain::API_KEY, "own-key")]),
                    &with_values(&[(hosted_brain::BASE, "kept-base")])
                )
                .is_empty(),
            "no set is complete in one source"
        );
    }

    /// A message names a list as `` `A`, `B` ``, and an `any_of` set by set.
    #[test]
    fn a_message_names_a_list_and_each_set_of_an_any_of() {
        let list: LaunchVariables = serde_json5::from_str(r#"["B_KEY", "A_KEY"]"#).unwrap();
        assert_eq!(list.describe(), "`A_KEY`, `B_KEY`");
        assert_eq!(
            hosted_brain::any_of().describe(),
            "one set of `OPENAI_COMPATIBLE_API_BASE` and `OPENAI_COMPATIBLE_API_KEY`, or of \
             `GEMINI_API_KEY`"
        );
        assert_eq!(
            any_of(r#"{ any_of: [["A_KEY", "B_KEY", "C_KEY"], ["D_KEY"], ["E_KEY"]] }"#).describe(),
            "one set of `A_KEY`, `B_KEY` and `C_KEY`, or of `D_KEY`, or of `E_KEY`"
        );
    }

    /// A name of any set of an `any_of` that `env_vars` also gives is found:
    /// whichever set is taken, no variable gets two values.
    #[test]
    fn a_name_of_any_set_that_env_vars_also_gives_is_found() {
        let env_vars = BTreeMap::from([("GEMINI_API_KEY".to_owned(), "x".to_owned())]);
        assert_eq!(
            names(hosted_brain::any_of().given_in(&env_vars)),
            ["GEMINI_API_KEY"]
        );
    }

    /// An instance with an `any_of` on a peer is refused, and the refusal
    /// names its sets.
    #[test]
    fn an_instance_with_an_any_of_is_refused_on_a_peer() {
        let mut brain = DeploymentInstance::empty(name("brain_inst"));
        brain.env_from_launch = hosted_brain::any_of();
        let placements = Placements::new(
            CoreNodeName::new("coordinator").unwrap(),
            BTreeMap::from([(
                "brain_inst".to_owned(),
                CoreNodeName::new("peer_b").unwrap(),
            )]),
        );
        let refusal = refuse_peer_placements([&brain], &placements, VariableChange::Launch)
            .expect("the brain runs on a peer");
        assert!(
            refusal.contains(
                "`brain_inst` takes one set of `OPENAI_COMPATIBLE_API_BASE` and \
                 `OPENAI_COMPATIBLE_API_KEY`, or of `GEMINI_API_KEY` from the launch, and the \
                 launch places it on `peer_b`"
            ),
            "{refusal}"
        );
    }

    /// The need of an instance that takes the `any_of` of the hosted brain.
    fn choice(instance: &str, starter: Starter) -> ChoiceNeed {
        ChoiceNeed {
            variables: hosted_brain::any_of(),
            instance: instance.to_owned(),
            starter,
        }
    }

    fn scoped_starter(features: &[&str], scopes: &[&str]) -> Starter {
        Starter::ScopedJoin {
            option: name("demo_arm"),
            features: features.iter().map(|feature| name(feature)).collect(),
            scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
        }
    }

    /// The needs of an instance: one per variable of a list, or one for a
    /// `any_of`, which no variable need stands for.
    #[test]
    fn an_instance_with_an_any_of_needs_a_set_and_no_variable() {
        let mut brain = DeploymentInstance::empty(name("brain_inst"));
        brain.env_from_launch = hosted_brain::any_of();
        let mut needs = VariableNeeds::default();
        needs.add_instance("brain_inst", &brain, &Starter::Launch);
        assert_eq!(needs.iter().count(), 0);
        assert_eq!(
            needs.choices().cloned().collect::<Vec<_>>(),
            [choice("brain_inst", Starter::Launch)]
        );
        assert!(!needs.is_empty());
    }

    /// The needs of an `any_of` merge their scopes and features as the needs
    /// of a variable do, and another `any_of` is another need.
    #[test]
    fn a_scoped_choice_keeps_its_smallest_feature_sets_and_every_scope() {
        let mut needs = VariableNeeds::default();
        needs.add_choice(choice(
            "brain_inst",
            scoped_starter(&["router"], &["controls_inst"]),
        ));
        needs.add_choice(choice(
            "brain_inst",
            scoped_starter(&["router", "camera"], &["controls_inst"]),
        ));
        needs.add_choice(choice(
            "brain_inst",
            scoped_starter(&["router"], &["applier_inst"]),
        ));
        let mut other = choice(
            "brain_inst",
            scoped_starter(&["router"], &["controls_inst"]),
        );
        other.variables = any_of(r#"{ any_of: [["A_KEY"], ["B_KEY"]] }"#);
        needs.add_choice(other.clone());

        assert_eq!(
            needs.choices().cloned().collect::<Vec<_>>(),
            [
                choice(
                    "brain_inst",
                    scoped_starter(&["router"], &["controls_inst", "applier_inst"])
                ),
                other,
            ]
        );
    }

    /// The needs of the same `any_of` are one group, in the order added.
    #[test]
    fn the_choice_reasons_group_the_needs_by_any_of() {
        let mut needs = VariableNeeds::default();
        needs.add_choice(choice("alpha_brain_inst", Starter::Launch));
        let mut other = choice("tool_inst", Starter::Launch);
        other.variables = any_of(r#"{ any_of: [["A_KEY"], ["B_KEY"]] }"#);
        needs.add_choice(other);
        needs.add_choice(choice(
            "brain_inst",
            scoped_starter(&[], &["framework_controls_inst"]),
        ));

        let grouped: Vec<(String, String)> = needs
            .choice_reasons()
            .into_iter()
            .map(|(variables, reasons)| (variables.describe(), reasons))
            .collect();
        assert_eq!(
            grouped,
            [
                (
                    "one set of `OPENAI_COMPATIBLE_API_BASE` and `OPENAI_COMPATIBLE_API_KEY`, or \
                     of `GEMINI_API_KEY`"
                        .to_owned(),
                    "`alpha_brain_inst` takes the first complete set from the launch, and the \
                     launch starts it; `brain_inst` of option `demo_arm` takes the first \
                     complete set from the launch, and stack.join through \
                     `framework_controls_inst` can add a copy of `demo_arm`"
                        .to_owned(),
                ),
                (
                    "one set of `A_KEY`, or of `B_KEY`".to_owned(),
                    "`tool_inst` takes the first complete set from the launch, and the launch \
                     starts it"
                        .to_owned(),
                ),
            ]
        );
    }

    /// An `any_of` passes the check when the first set, the second set or
    /// both are complete, and no set is held in part. It fails the check
    /// when no set is complete, and when a set is held in part, also beside
    /// a complete set.
    #[test]
    fn an_any_of_passes_the_check_with_a_complete_set_and_no_set_held_in_part() {
        let mut needs = VariableNeeds::default();
        needs.add_choice(choice("brain_inst", Starter::Launch));
        for held in [
            &["OPENAI_COMPATIBLE_API_BASE", "OPENAI_COMPATIBLE_API_KEY"][..],
            &["GEMINI_API_KEY"][..],
            &[
                "OPENAI_COMPATIBLE_API_BASE",
                "OPENAI_COMPATIBLE_API_KEY",
                "GEMINI_API_KEY",
            ][..],
        ] {
            needs
                .check(ChangeValues::launch(&values(held)))
                .unwrap_or_else(|refusal| panic!("{held:?}: {refusal}"));
        }
        for held in [
            &[][..],
            &["OPENAI_COMPATIBLE_API_KEY"][..],
            &["OPENAI_COMPATIBLE_API_BASE", "GEMINI_API_KEY"][..],
        ] {
            needs
                .check(ChangeValues::launch(&values(held)))
                .expect_err("no complete set, or a set held in part");
        }
    }

    /// The sets held in part: of each set of two variables or more, those of
    /// which the environment holds some variables and not all, in order,
    /// each with the variables that it does not hold. A list has none.
    #[test]
    fn the_sets_held_in_part_name_their_missing_variables() {
        let three =
            any_of(r#"{ any_of: [["C_KEY", "A_KEY", "B_KEY"], ["D_KEY"], ["E_KEY", "F_KEY"]] }"#);
        let in_part = |held: &[&str]| {
            three
                .sets_held_in_part(&values(held))
                .into_iter()
                .map(|held| {
                    format!(
                        "{} without {}",
                        quoted_set(&held.set),
                        quoted_series(&held.missing, "and")
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            in_part(&["B_KEY", "D_KEY", "E_KEY"]),
            [
                "`A_KEY`, `B_KEY` and `C_KEY` without `A_KEY` and `C_KEY`",
                "`E_KEY` and `F_KEY` without `F_KEY`",
            ]
        );
        assert!(in_part(&["A_KEY", "B_KEY", "C_KEY", "D_KEY"]).is_empty());
        assert!(in_part(&[]).is_empty());
        let list: LaunchVariables = serde_json5::from_str(r#"["A_KEY", "B_KEY"]"#).unwrap();
        assert!(list.sets_held_in_part(&values(&["A_KEY"])).is_empty());
    }

    /// The refusal of a set held in part names the set and its missing
    /// variables, beside a complete set and with no complete set, in the
    /// launch and in the join. No value is in the refusal.
    #[test]
    fn the_refusal_of_a_set_held_in_part_names_its_missing_variables() {
        let mut needs = VariableNeeds::default();
        needs.add_choice(choice("brain_inst", Starter::Launch));
        assert_eq!(
            needs
                .check(ChangeValues::launch(&values(&[
                    "OPENAI_COMPATIBLE_API_BASE",
                    "GEMINI_API_KEY"
                ])))
                .expect_err("the first set is held in part")
                .to_string(),
            "the launch needs one set of `OPENAI_COMPATIBLE_API_BASE` and \
             `OPENAI_COMPATIBLE_API_KEY`, or of `GEMINI_API_KEY`, and the launch environment \
             holds the set of `OPENAI_COMPATIBLE_API_BASE` and `OPENAI_COMPATIBLE_API_KEY` in \
             part, as it does not hold `OPENAI_COMPATIBLE_API_KEY`, or holds it empty or with \
             characters that a node cannot receive: `brain_inst` takes the first complete set \
             from the launch, and the launch starts it. A launch and a join refuse a set that \
             their own environment holds in part, also when another set is complete, so that an \
             instance never takes another set in its place: export every variable of such a set, \
             or none of them, in the shell that launches. A value must not be empty, and must \
             hold letters, digits and `_ - . / : , = @ + %` only"
        );
        assert_eq!(
            needs
                .check(ChangeValues::launch(&values(&[
                    "OPENAI_COMPATIBLE_API_KEY"
                ])))
                .expect_err("the first set is held in part, and no set is complete")
                .to_string(),
            "the launch needs one set of `OPENAI_COMPATIBLE_API_BASE` and \
             `OPENAI_COMPATIBLE_API_KEY`, or of `GEMINI_API_KEY`, and the launch environment \
             holds the set of `OPENAI_COMPATIBLE_API_BASE` and `OPENAI_COMPATIBLE_API_KEY` in \
             part, as it does not hold `OPENAI_COMPATIBLE_API_BASE`, or holds it empty or with \
             characters that a node cannot receive: `brain_inst` takes the first complete set \
             from the launch, and the launch starts it. Export every variable of one set in the \
             shell that launches. A launch and a join refuse a set that their own environment \
             holds in part, also when another set is complete, so that an instance never takes \
             another set in its place: export every variable of such a set, or none of them, in \
             the shell that launches. A value must not be empty, and must hold letters, digits \
             and `_ - . / : , = @ + %` only"
        );

        let mut joined = VariableNeeds::default();
        let mut tool = choice("alpha_tool_inst", Starter::Join);
        tool.variables = any_of(r#"{ any_of: [["A_KEY", "B_KEY", "C_KEY"], ["D_KEY", "E_KEY"]] }"#);
        joined.add_choice(tool);
        for kept in [values(&[]), values(&["B_KEY", "C_KEY"])] {
            assert_eq!(
                joined
                    .check(ChangeValues::join(
                        &values(&["A_KEY", "D_KEY", "E_KEY"]),
                        &kept
                    ))
                    .expect_err("the join environment holds the first set in part")
                    .to_string(),
                "the join needs one set of `A_KEY`, `B_KEY` and `C_KEY`, or of `D_KEY` and \
                 `E_KEY`, and the join environment holds the set of `A_KEY`, `B_KEY` and `C_KEY` \
                 in part, as it does not hold `B_KEY` and `C_KEY`, or holds them empty or with \
                 characters that a node cannot receive: `alpha_tool_inst` takes the first \
                 complete set from the launch, and the join starts it. A launch and a join refuse \
                 a set that their own environment holds in part, also when another set is \
                 complete, so that an instance never takes another set in its place: export \
                 every variable of such a set, or none of them, in the shell that joins. A value \
                 must not be empty, and must hold letters, digits and `_ - . / : , = @ + %` only"
            );
        }
        joined
            .check(ChangeValues::join(
                &values(&[]),
                &values(&["A_KEY", "B_KEY", "C_KEY"]),
            ))
            .expect("the kept values hold the first set complete");
    }

    /// A missing variable of a list and an `any_of` with a set held in part
    /// beside a complete set are one refusal, one line each: the line of the
    /// `any_of` names the set held in part and its missing variable.
    #[test]
    fn a_missing_variable_and_a_set_held_in_part_are_one_refusal() {
        let mut needs = VariableNeeds::default();
        needs.add(VariableNeed {
            variable: variable("TOOL_KEY"),
            instance: "tool_inst".to_owned(),
            starter: Starter::Launch,
        });
        needs.add_choice(choice("brain_inst", Starter::Launch));
        assert_eq!(
            needs
                .check(ChangeValues::launch(&values(&[
                    "OPENAI_COMPATIBLE_API_KEY",
                    "GEMINI_API_KEY"
                ])))
                .expect_err("a missing variable and a set held in part")
                .to_string(),
            "the launch needs variables that the launch environment does not hold, or holds \
             empty or with characters that a node cannot receive:\n  - `TOOL_KEY`: `tool_inst` \
             takes it from the launch, and the launch starts it\n  - one set of \
             `OPENAI_COMPATIBLE_API_BASE` and `OPENAI_COMPATIBLE_API_KEY`, or of \
             `GEMINI_API_KEY`, with the set of `OPENAI_COMPATIBLE_API_BASE` and \
             `OPENAI_COMPATIBLE_API_KEY` held in part in the launch environment, without \
             `OPENAI_COMPATIBLE_API_BASE`: `brain_inst` takes the first complete set from the \
             launch, and the launch starts it.\nExport each variable in the shell that launches. \
             A launch and a join refuse a set that their own environment holds in part, also \
             when another set is complete, so that an instance never takes another set in its \
             place: export every variable of such a set, or none of them, in the shell that \
             launches. A value must not be empty, and must hold letters, digits and \
             `_ - . / : , = @ + %` only"
        );
    }

    /// The refusal of one `any_of` names every set, each instance that takes
    /// one and what starts it, in the launch and in the join.
    #[test]
    fn the_refusal_of_one_any_of_names_every_set() {
        let mut needs = VariableNeeds::default();
        needs.add_choice(choice("brain_inst", Starter::Launch));
        assert_eq!(
            needs
                .check(ChangeValues::launch(&values(&[])))
                .expect_err("no set")
                .to_string(),
            "the launch needs one set of `OPENAI_COMPATIBLE_API_BASE` and \
             `OPENAI_COMPATIBLE_API_KEY`, or of `GEMINI_API_KEY`, and the launch environment \
             holds no set complete, or holds a variable of it empty or with characters that a \
             node cannot receive: `brain_inst` takes the first complete set from the launch, and \
             the launch starts it. Export every variable of one set in the shell that launches. \
             A value must not be empty, and must hold letters, digits and `_ - . / : , = @ + %` \
             only"
        );

        let mut joined = VariableNeeds::default();
        joined.add_choice(choice("alpha_brain_inst", Starter::Join));
        assert_eq!(
            joined
                .check(ChangeValues::join(&values(&[]), &values(&[])))
                .expect_err("no set")
                .to_string(),
            "the join needs one set of `OPENAI_COMPATIBLE_API_BASE` and \
             `OPENAI_COMPATIBLE_API_KEY`, or of `GEMINI_API_KEY`, and neither the join \
             environment nor the launch environment holds a set complete, or they hold a \
             variable of it empty or with characters that a node cannot receive: \
             `alpha_brain_inst` takes the first complete set from the launch, and the join \
             starts it. Export every variable of one set in the shell that joins or in the shell \
             that launches. A value must not be empty, and must hold letters, digits and \
             `_ - . / : , = @ + %` only"
        );
    }

    /// A missing variable of a list and an `any_of` with no complete set are
    /// one refusal, one line each.
    #[test]
    fn a_missing_variable_and_an_unmet_any_of_are_one_refusal() {
        let mut needs = VariableNeeds::default();
        needs.add(VariableNeed {
            variable: variable("TOOL_KEY"),
            instance: "tool_inst".to_owned(),
            starter: Starter::Launch,
        });
        needs.add_choice(choice("brain_inst", Starter::Launch));
        assert_eq!(
            needs
                .check(ChangeValues::launch(&values(&[])))
                .expect_err("nothing held")
                .to_string(),
            "the launch needs variables that the launch environment does not hold, or holds \
             empty or with characters that a node cannot receive:\n  - `TOOL_KEY`: `tool_inst` \
             takes it from the launch, and the launch starts it\n  - one set of \
             `OPENAI_COMPATIBLE_API_BASE` and `OPENAI_COMPATIBLE_API_KEY`, or of \
             `GEMINI_API_KEY`: `brain_inst` takes the first complete set from the launch, and \
             the launch starts it.\nExport each variable, and every variable of one set, in the \
             shell that launches. A value must not be empty, and must hold letters, digits and \
             `_ - . / : , = @ + %` only"
        );

        let mut two = VariableNeeds::default();
        two.add_choice(choice("brain_inst", Starter::Launch));
        let mut other = choice("tool_inst", Starter::Launch);
        other.variables = any_of(r#"{ any_of: [["A_KEY"], ["B_KEY"]] }"#);
        two.add_choice(other);
        let refusal = two
            .check(ChangeValues::launch(&values(&[])))
            .expect_err("nothing held")
            .to_string();
        assert!(
            refusal.contains("\n  - one set of `A_KEY`, or of `B_KEY`: `tool_inst`")
                && refusal.contains(
                    "\nExport every variable of one set of each in the shell that launches."
                ),
            "{refusal}"
        );
    }
    /// The `env_from_launch` of the instance `instance` in launcher.json5,
    /// which writes `variables`.
    fn entry<'a>(instance: &'a str, variables: &'a LaunchVariables) -> WrittenLaunchVariables<'a> {
        WrittenLaunchVariables {
            variables,
            instance,
            key: LaunchVariablesKey::EnvFromLaunch,
            document: "launcher.json5",
        }
    }

    /// A variable of a set of two variables or more is refused in a list and
    /// in another set of another `any_of`, a set of one variable included,
    /// whatever their order, and the refusal names the variable, the set,
    /// the other one and where each one is written.
    #[test]
    fn a_variable_of_a_set_of_two_or_more_is_refused_outside_that_set() {
        let brain = any_of(r#"{ any_of: [["OTHER_KEY", "TOOL_KEY"], ["PROBE_KEY"]] }"#);
        let tool: LaunchVariables = serde_json5::from_str(r#"["TOOL_KEY"]"#).unwrap();
        let refusal = |written: &[WrittenLaunchVariables<'_>]| {
            refuse_variables_outside_their_set(written)
                .expect_err("a variable outside its set")
                .to_string()
        };
        let expected = "`TOOL_KEY` is in the set of `OTHER_KEY` and `TOOL_KEY` of an `any_of` \
                        (the `env_from_launch` of `brain_inst` in launcher.json5) and in a list \
                        (the `env_from_launch` of `tool_inst` in launcher.json5): an environment \
                        that holds `TOOL_KEY` for the second holds the set in part, which a \
                        launch and a join refuse, or complete, and then the instance of the set \
                        takes the value of the second; write a variable of a set of two \
                        variables or more in that set only, or in the same set of another \
                        `any_of`";
        assert_eq!(
            refusal(&[entry("brain_inst", &brain), entry("tool_inst", &tool)]),
            expected
        );
        assert_eq!(
            refusal(&[entry("tool_inst", &tool), entry("brain_inst", &brain)]),
            expected
        );

        for (other, variable, named) in [
            (
                r#"{ any_of: [["TOOL_KEY"], ["A_KEY"]] }"#,
                "TOOL_KEY",
                "the set of `TOOL_KEY` of an `any_of`",
            ),
            (
                r#"{ any_of: [["A_KEY"], ["TOOL_KEY", "B_KEY"]] }"#,
                "TOOL_KEY",
                "the set of `B_KEY` and `TOOL_KEY` of an `any_of`",
            ),
            (
                r#"{ any_of: [["OTHER_KEY", "TOOL_KEY", "C_KEY"], ["A_KEY"]] }"#,
                "OTHER_KEY",
                "the set of `C_KEY`, `OTHER_KEY` and `TOOL_KEY` of an `any_of`",
            ),
        ] {
            let other = any_of(other);
            let refused = refusal(&[entry("brain_inst", &brain), entry("planner_inst", &other)]);
            assert!(
                refused.starts_with(&format!(
                    "`{variable}` is in the set of `OTHER_KEY` and `TOOL_KEY` of an `any_of` (the \
                     `env_from_launch` of `brain_inst` in launcher.json5) and in {named} (the \
                     `env_from_launch` of `planner_inst` in launcher.json5): "
                )),
                "{refused}"
            );
        }
    }

    /// Two `any_of`s that share a whole set, a list or another set that
    /// names a variable of a set of one variable, and two instances of one
    /// `any_of` pass.
    #[test]
    fn a_whole_set_and_a_set_of_one_variable_can_be_shared() {
        let brain = hosted_brain::any_of();
        let planner = any_of(
            r#"{ any_of: [["OPENAI_COMPATIBLE_API_BASE", "OPENAI_COMPATIBLE_API_KEY"], ["OPENROUTER_API_KEY"]] }"#,
        );
        let provider_key_list: LaunchVariables =
            serde_json5::from_str(r#"["GEMINI_API_KEY", "TOOL_KEY"]"#).unwrap();
        let provider_key_set = any_of(r#"{ any_of: [["GEMINI_API_KEY"], ["TOOL_KEY"]] }"#);
        refuse_variables_outside_their_set(&[
            entry("alpha_brain_inst", &brain),
            entry("bravo_brain_inst", &brain),
            entry("planner_inst", &planner),
            entry("tool_inst", &provider_key_list),
            entry("probe_inst", &provider_key_set),
        ])
        .expect("no variable of a set of two is outside that set");
    }

    /// A launch whose values hold a set of an `any_of` of its launcher in
    /// part is refused, each set once, with the first one that writes it;
    /// with every set complete or held not at all, it passes.
    #[test]
    fn the_kept_values_hold_no_set_in_part() {
        let brain = hosted_brain::any_of();
        let tool = any_of(r#"{ any_of: [["A_KEY", "B_KEY"], ["C_KEY"]] }"#);
        let written = [
            entry("alpha_brain_inst", &brain),
            entry("bravo_brain_inst", &brain),
            entry("tool_inst", &tool),
        ];
        for held in [
            &[][..],
            &["GEMINI_API_KEY", "C_KEY"][..],
            &[
                "OPENAI_COMPATIBLE_API_BASE",
                "OPENAI_COMPATIBLE_API_KEY",
                "A_KEY",
                "B_KEY",
            ][..],
        ] {
            refuse_kept_sets_held_in_part(&written, &values(held))
                .unwrap_or_else(|refusal| panic!("{held:?}: {refusal}"));
        }
        assert_eq!(
            refuse_kept_sets_held_in_part(
                &written,
                &values(&["OPENAI_COMPATIBLE_API_KEY", "GEMINI_API_KEY"])
            )
            .expect_err("the set of the API is held in part")
            .to_string(),
            "the launch environment holds the set of `OPENAI_COMPATIBLE_API_BASE` and \
             `OPENAI_COMPATIBLE_API_KEY` in part, as it does not hold \
             `OPENAI_COMPATIBLE_API_BASE`, or holds it empty or with characters that a node \
             cannot receive: the `env_from_launch` of `alpha_brain_inst` in launcher.json5 names \
             it in one set of `OPENAI_COMPATIBLE_API_BASE` and `OPENAI_COMPATIBLE_API_KEY`, or \
             of `GEMINI_API_KEY`, and the launch keeps the value of each launch variable of its \
             launcher for the joins. A launch and a join refuse a set that their own environment \
             holds in part, also when another set is complete, so that an instance never takes \
             another set in its place: export every variable of such a set, or none of them, in \
             the shell that launches. A value must not be empty, and must hold letters, digits \
             and `_ - . / : , = @ + %` only"
        );
        assert_eq!(
            refuse_kept_sets_held_in_part(
                &written,
                &values(&["OPENAI_COMPATIBLE_API_BASE", "B_KEY"])
            )
            .expect_err("two sets held in part")
            .to_string(),
            "the launch environment holds these sets in part, or holds a variable of them empty \
             or with characters that a node cannot receive:\n  - the set of \
             `OPENAI_COMPATIBLE_API_BASE` and `OPENAI_COMPATIBLE_API_KEY`, without \
             `OPENAI_COMPATIBLE_API_KEY`: the `env_from_launch` of `alpha_brain_inst` in \
             launcher.json5 names it in one set of `OPENAI_COMPATIBLE_API_BASE` and \
             `OPENAI_COMPATIBLE_API_KEY`, or of `GEMINI_API_KEY`\n  - the set of `A_KEY` and \
             `B_KEY`, without `A_KEY`: the `env_from_launch` of `tool_inst` in launcher.json5 \
             names it in one set of `A_KEY` and `B_KEY`, or of `C_KEY`.\nThe launch keeps the \
             value of each launch variable of its launcher for the joins. A launch and a join \
             refuse a set that their own environment holds in part, also when another set is \
             complete, so that an instance never takes another set in its place: export every \
             variable of such a set, or none of them, in the shell that launches. A value must \
             not be empty, and must hold letters, digits and `_ - . / : , = @ + %` only"
        );
    }
}
