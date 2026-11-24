//! Launch variables: the environment variables that an instance takes from
//! the environment of the launch, by name. A launcher names them in the
//! `env_from_launch` of an instance and with a `set_env_from_launch`
//! adjustment, so no launcher file holds a value: a list, or a `one_of` of
//! sets ([`LaunchVariables`]).
//!
//! A launch and a join parse the environment that the CLI forwarded with
//! them against the launch variables of the launcher ([`ForwardedEnv`]): the
//! value of a launch variable reaches the run of each instance that takes it,
//! and no other node, add or build. An instance takes each variable of its
//! list, or the variables of the first set of its `one_of` that the
//! environment of the change holds complete. The daemon keeps, in memory,
//! the values of the launch ([`LaunchValues`]), and a join gives each
//! instance that it starts the kept value of each variable that the instance
//! takes and the join does not forward: a join reads its own forwarded
//! environment, then the kept values, for a set as for a value. The launch
//! is refused when an instance that the launch or a join through a
//! `robot_membership` scope can start takes a variable that the forwarded
//! environment of the launch does not hold, or a `one_of` none of whose sets
//! it holds complete, and a join when an instance that it starts takes a
//! variable or a `one_of` that neither its own forwarded environment nor the
//! kept values hold, or hold complete ([`VariableNeeds::check`]).
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

/// A `one_of` that names no set an instance can take, or a set that is never
/// chosen. Each message names the set by its place in the list, from 1.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrokenOneOf {
    #[error(
        "`one_of` names no set; an instance that takes no variable from the launch writes `[]`"
    )]
    NoSet,
    #[error("`one_of` names one set; write that set as a list")]
    OneSet,
    #[error("set {set} of `one_of` is empty; each set names at least one variable")]
    EmptySet { set: usize },
    #[error("set {set} of `one_of` names `{variable}` two times; a set names each variable once")]
    RepeatedInSet {
        set: usize,
        variable: LaunchVariable,
    },
    #[error(
        "set {later} of `one_of` holds every variable of set {earlier}, which comes before it, so \
         it is never chosen; take it out, or put it before set {earlier}"
    )]
    NeverChosen { earlier: usize, later: usize },
}

/// The key of the `one_of` form of a list of launch variables.
const ONE_OF: &str = "one_of";

/// The variables that one instance takes from the launch environment, in one
/// of two forms.
///
/// A list, `["A", "B"]`, names variables that the instance takes each one
/// of. It is a set: two lists that name the same variables in another order
/// are the same, and a list writes back in name order. An empty list is the
/// same as no list.
///
/// A `one_of`, `{ one_of: [["A", "B"], ["C"]] }`, names two sets or more, in
/// order: of these, [`Self::taken`] gives the first set that the environment
/// holds complete. A name can be in several sets. Each set writes back in
/// name order, and the sets in the order written. The parse refuses a
/// `one_of` that [`BrokenOneOf`] names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchVariables(Form);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Form {
    /// A list: each variable is taken.
    Each(BTreeSet<LaunchVariable>),
    /// A `one_of`: the first complete set is taken.
    OneOf(Vec<BTreeSet<LaunchVariable>>),
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

impl TryFrom<Vec<LaunchVariable>> for LaunchVariables {
    type Error = RepeatedLaunchVariable;

    fn try_from(written: Vec<LaunchVariable>) -> Result<Self, Self::Error> {
        set_of(written)
            .map(|variables| Self(Form::Each(variables)))
            .map_err(|variable| RepeatedLaunchVariable { variable })
    }
}

impl LaunchVariables {
    /// The `one_of` of `written`, its sets in order: two sets or more, none
    /// empty, each naming a variable once, and none holding every variable
    /// of a set before it, which would be complete first.
    pub fn one_of(written: Vec<Vec<LaunchVariable>>) -> Result<Self, BrokenOneOf> {
        match written.len() {
            0 => return Err(BrokenOneOf::NoSet),
            1 => return Err(BrokenOneOf::OneSet),
            _ => {}
        }
        let mut sets: Vec<BTreeSet<LaunchVariable>> = Vec::with_capacity(written.len());
        for (index, variables) in written.into_iter().enumerate() {
            let set = index + 1;
            if variables.is_empty() {
                return Err(BrokenOneOf::EmptySet { set });
            }
            let variables = set_of(variables)
                .map_err(|variable| BrokenOneOf::RepeatedInSet { set, variable })?;
            if let Some(earlier) = sets.iter().position(|before| before.is_subset(&variables)) {
                return Err(BrokenOneOf::NeverChosen {
                    earlier: earlier + 1,
                    later: set,
                });
            }
            sets.push(variables);
        }
        Ok(Self(Form::OneOf(sets)))
    }

    /// Whether the instance takes no variable: an empty list.
    pub fn is_empty(&self) -> bool {
        matches!(&self.0, Form::Each(variables) if variables.is_empty())
    }

    /// Every variable that the instance can take, each once, in name order:
    /// each variable of a list, or the variables of every set of a `one_of`.
    pub fn iter(&self) -> impl Iterator<Item = &LaunchVariable> {
        let names: BTreeSet<&LaunchVariable> = match &self.0 {
            Form::Each(variables) => variables.iter().collect(),
            Form::OneOf(sets) => sets.iter().flatten().collect(),
        };
        names.into_iter()
    }

    /// The variables that the instance takes each one of, in name order: the
    /// variables of a list, and none for a `one_of`.
    pub fn required(&self) -> impl Iterator<Item = &LaunchVariable> {
        let list = match &self.0 {
            Form::Each(variables) => Some(variables),
            Form::OneOf(_) => None,
        };
        list.into_iter().flatten()
    }

    /// The sets of a `one_of`, in order, and none for a list.
    pub fn sets(&self) -> &[BTreeSet<LaunchVariable>] {
        match &self.0 {
            Form::Each(_) => &[],
            Form::OneOf(sets) => sets,
        }
    }

    /// The variables that the instance takes, in name order, when `holds`
    /// tells which variables the environment holds: each variable of a list,
    /// or the variables of the first set of a `one_of` that `holds` holds
    /// complete, and none when no set is complete.
    pub fn taken(&self, holds: impl Fn(&str) -> bool) -> Vec<&LaunchVariable> {
        match &self.0 {
            Form::Each(variables) => variables.iter().collect(),
            Form::OneOf(sets) => sets
                .iter()
                .find(|set| set.iter().all(|variable| holds(variable.as_str())))
                .map_or_default(|set| set.iter().collect()),
        }
    }

    /// The variables as a message names them: a list as `` `A`, `B` ``, and
    /// a `one_of` as "one set of `A` and `B`, or of `C`".
    pub fn describe(&self) -> String {
        match &self.0 {
            Form::Each(variables) => crate::error::format_quoted_list(variables),
            Form::OneOf(sets) => {
                let sets: Vec<String> = sets
                    .iter()
                    .map(|set| quoted_series(&set.iter().collect::<Vec<_>>(), "and"))
                    .collect();
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

/// A list writes as a list of names, and a `one_of` as an object with the
/// one key `one_of`.
impl Serialize for LaunchVariables {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match &self.0 {
            Form::Each(variables) => serializer.collect_seq(variables),
            Form::OneOf(sets) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry(ONE_OF, sets)?;
                map.end()
            }
        }
    }
}

/// A list reads as [`LaunchVariables::try_from`], and an object as
/// [`LaunchVariables::one_of`]. Each name is a [`LaunchVariable`], and every
/// refusal keeps the text of its error.
impl<'de> Deserialize<'de> for LaunchVariables {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(LaunchVariablesVisitor)
    }
}

/// The object form as it is written: the key `one_of` and no other.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WrittenOneOf {
    one_of: Vec<Vec<LaunchVariable>>,
}

struct LaunchVariablesVisitor;

impl<'de> serde::de::Visitor<'de> for LaunchVariablesVisitor {
    type Value = LaunchVariables;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a list of variable names, or an object `{ one_of: [[...], [...]] }`")
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
        let written =
            Vec::<LaunchVariable>::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))?;
        LaunchVariables::try_from(written).map_err(serde::de::Error::custom)
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        let WrittenOneOf { one_of } =
            WrittenOneOf::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
        LaunchVariables::one_of(one_of).map_err(serde::de::Error::custom)
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
    /// `shown`. A `one_of` is a [`ChoiceNeed`].
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

/// One instance that takes the first complete set of a `one_of`, and what
/// can start it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceNeed {
    /// The `one_of` that the instance takes a set of.
    pub variables: LaunchVariables,
    /// The instance: its id in the stack, or its id in the copy for an
    /// instance of a copy that a join through a scope can add.
    pub instance: String,
    pub starter: Starter,
}

impl ChoiceNeed {
    /// The need of `instance`, named `shown`, when it takes a `one_of`.
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

/// The instances that take launch variables, each with what can start it:
/// one entry per variable of a list ([`VariableNeed`]), and one per
/// `one_of` ([`ChoiceNeed`]).
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

    /// Adds the need of a `one_of`, as [`add_need`] says.
    pub fn add_choice(&mut self, need: ChoiceNeed) {
        add_need(&mut self.choices, need);
    }

    /// Adds the needs of `instance`, named `shown`, started by `starter`: one
    /// for each variable of its list, or one for its `one_of`.
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

    /// Adds every need of `other`, of a variable and of a `one_of`.
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

    /// The needs of `one_of`s.
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

    /// Each `one_of` of these needs, in the order it is first added, with
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

    /// The refusal of `change` when a variable of a list of these needs is
    /// one that `holds` does not hold, or when `holds` holds no set of a
    /// `one_of` complete: it names each such variable and `one_of`, each
    /// instance that needs it and what can start that instance.
    pub fn check(
        &self,
        change: VariableChange,
        holds: impl Fn(&str) -> bool,
    ) -> Result<(), MissingLaunchVariables> {
        let missing: BTreeMap<LaunchVariable, String> = self
            .reasons()
            .into_iter()
            .filter(|(variable, _)| !holds(variable.as_str()))
            .collect();
        let unmet: Vec<(LaunchVariables, String)> = self
            .choice_reasons()
            .into_iter()
            .filter(|(variables, _)| variables.taken(&holds).is_empty())
            .collect();
        if missing.is_empty() && unmet.is_empty() {
            Ok(())
        } else {
            Err(MissingLaunchVariables {
                change,
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

impl std::fmt::Display for VariableChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Launch => "the launch",
            Self::Join => "the join",
        })
    }
}

/// A launch or a join refused because one of its instances takes a launch
/// variable that its environment does not hold, or a `one_of` none of whose
/// sets its environment holds complete. The CLI does not forward a value
/// with characters that a node cannot receive, and the daemon counts an
/// empty value as no value, so the refusal also names these causes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingLaunchVariables {
    change: VariableChange,
    /// Each missing variable of a list, with what needs it
    /// ([`VariableNeeds::reasons`]).
    missing: BTreeMap<LaunchVariable, String>,
    /// Each `one_of` with no complete set, with what needs it
    /// ([`VariableNeeds::choice_reasons`]).
    unmet: Vec<(LaunchVariables, String)>,
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

    /// The refusal of one `one_of` and no missing variable of a list.
    fn write_one_choice(
        &self,
        f: &mut std::fmt::Formatter<'_>,
        variables: &LaunchVariables,
        reasons: &str,
    ) -> std::fmt::Result {
        let environment = match self.change {
            VariableChange::Launch => {
                "the launch environment holds no set complete, or holds a variable of it"
            }
            VariableChange::Join => {
                "neither the join environment nor the launch environment holds a set complete, \
                 or they hold a variable of it"
            }
        };
        write!(
            f,
            "{} needs {}, and {environment} empty or with characters that a node cannot \
             receive: {reasons}. Export every variable of one set in {}. A value must not be \
             empty, and must hold {SAFE_ENV_VALUE_CHARACTERS} only",
            self.change,
            variables.describe(),
            self.shell(),
        )
    }

    /// The refusal of several `one_of`s, or of `one_of`s and missing
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
            .chain(
                self.unmet
                    .iter()
                    .map(|(variables, reasons)| format!("{}: {reasons}", variables.describe())),
            );
        let of_each_set = if self.unmet.len() == 1 {
            "every variable of one set"
        } else {
            "every variable of one set of each"
        };
        let export = if self.missing.is_empty() {
            format!("Export {of_each_set}")
        } else {
            format!("Export each variable, and {of_each_set},")
        };
        write!(
            f,
            "{} needs variables that {environment}:{}.\n{export} in {}. A value must not be \
             empty, and must hold {SAFE_ENV_VALUE_CHARACTERS} only",
            self.change,
            crate::error::format_bulleted(lines),
            self.shell(),
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
            (true, [(variables, reasons)]) => self.write_one_choice(f, variables, reasons),
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
            .check(VariableChange::Launch, |_| false)
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
        needs.check(VariableChange::Launch, |_| true).unwrap();
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
            .check(VariableChange::Join, |name| name == "C_KEY")
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

    /// The `one_of` of the hosted brain: the API's two variables, else the
    /// key of the provider.
    const BRAIN_ONE_OF: &str = r#"{ one_of: [
        ["OPENAI_COMPATIBLE_API_KEY", "OPENAI_COMPATIBLE_API_BASE"],
        ["GEMINI_API_KEY"],
    ] }"#;

    fn one_of(written: &str) -> LaunchVariables {
        serde_json5::from_str(written).expect(written)
    }

    fn names<'a>(variables: impl IntoIterator<Item = &'a LaunchVariable>) -> Vec<&'a str> {
        variables.into_iter().map(LaunchVariable::as_str).collect()
    }

    /// A `one_of` keeps its sets in the order written, each set in name
    /// order, and writes back as an object with the one key `one_of`. Every
    /// name of every set is a variable that the instance can take, and none
    /// is one that it takes each time.
    #[test]
    fn a_one_of_keeps_its_sets_in_order_and_writes_back_as_an_object() {
        let parsed = one_of(BRAIN_ONE_OF);
        assert_eq!(
            parsed.sets().iter().map(names).collect::<Vec<_>>(),
            [
                vec!["OPENAI_COMPATIBLE_API_BASE", "OPENAI_COMPATIBLE_API_KEY"],
                vec!["GEMINI_API_KEY"],
            ]
        );
        assert_eq!(
            serde_json5::to_string(&parsed).unwrap(),
            r#"{"one_of":[["OPENAI_COMPATIBLE_API_BASE","OPENAI_COMPATIBLE_API_KEY"],["GEMINI_API_KEY"]]}"#
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

    /// The same sets in another order are another `one_of`, and a `one_of`
    /// is never the list of its names.
    #[test]
    fn the_order_of_the_sets_is_part_of_a_one_of() {
        let first = one_of(r#"{ one_of: [["A_KEY"], ["B_KEY"]] }"#);
        let second = one_of(r#"{ one_of: [["B_KEY"], ["A_KEY"]] }"#);
        let list: LaunchVariables = serde_json5::from_str(r#"["A_KEY", "B_KEY"]"#).unwrap();
        assert_ne!(first, second);
        assert_ne!(first, list);
        assert_eq!(first, one_of(r#"{ one_of: [["A_KEY"], ["B_KEY"]] }"#));
    }

    /// A name can be in several sets, and a set after a larger one that holds
    /// it is chosen when the larger one is not complete.
    #[test]
    fn a_name_can_be_in_several_sets() {
        let shared =
            one_of(r#"{ one_of: [["TOOL_KEY", "A_KEY", "B_KEY"], ["TOOL_KEY", "C_KEY"]] }"#);
        assert_eq!(
            names(shared.iter()),
            ["A_KEY", "B_KEY", "C_KEY", "TOOL_KEY"]
        );
        one_of(r#"{ one_of: [["A_KEY", "B_KEY"], ["A_KEY"]] }"#);
    }

    /// Each `one_of` that an instance cannot use is refused at the parse, and
    /// the message names the set by its place, from 1.
    #[test]
    fn a_broken_one_of_is_refused_naming_the_set() {
        for (written, expected) in [
            (
                r"{ one_of: [] }",
                "`one_of` names no set; an instance that takes no variable from the launch \
                 writes `[]`",
            ),
            (
                r#"{ one_of: [["A_KEY"]] }"#,
                "`one_of` names one set; write that set as a list",
            ),
            (
                r#"{ one_of: [["A_KEY"], []] }"#,
                "set 2 of `one_of` is empty; each set names at least one variable",
            ),
            (
                r#"{ one_of: [["A_KEY", "B_KEY", "A_KEY"], ["C_KEY"]] }"#,
                "set 1 of `one_of` names `A_KEY` two times; a set names each variable once",
            ),
            (
                r#"{ one_of: [["A_KEY"], ["B_KEY", "A_KEY"]] }"#,
                "set 2 of `one_of` holds every variable of set 1, which comes before it, so it \
                 is never chosen; take it out, or put it before set 1",
            ),
            (
                r#"{ one_of: [["C_KEY"], ["A_KEY", "B_KEY"], ["B_KEY", "A_KEY"]] }"#,
                "set 3 of `one_of` holds every variable of set 2, which comes before it, so it \
                 is never chosen; take it out, or put it before set 2",
            ),
            (
                r#"{ one_of: [["has-dash"], ["B_KEY"]] }"#,
                "`has-dash` is not a valid shell identifier",
            ),
            (
                r#"{ one_of: [["TMPDIR"], ["B_KEY"]] }"#,
                "`TMPDIR` is never forwarded by the CLI",
            ),
            (
                r#"{ one_of: [["A_KEY"], ["B_KEY"]], all: ["C_KEY"] }"#,
                "unknown field `all`",
            ),
            (r"{}", "missing field `one_of`"),
            (r#""A_KEY""#, "a list of variable names, or an object"),
        ] {
            let error = serde_json5::from_str::<LaunchVariables>(written)
                .expect_err(written)
                .to_string();
            assert!(error.contains(expected), "{written}: {error}");
        }
    }

    /// The variables taken: each one of a list whatever the environment
    /// holds, and of a `one_of` the first complete set alone, or none.
    #[test]
    fn the_first_complete_set_is_taken() {
        let brain = one_of(BRAIN_ONE_OF);
        let holding = |held: &'static [&'static str]| move |name: &str| held.contains(&name);
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
            assert_eq!(names(brain.taken(holding(held))), expected, "{held:?}");
        }
        let list: LaunchVariables = serde_json5::from_str(r#"["B_KEY", "A_KEY"]"#).unwrap();
        assert_eq!(names(list.taken(|_| false)), ["A_KEY", "B_KEY"]);
    }

    /// A message names a list as today, and a `one_of` set by set.
    #[test]
    fn a_message_names_a_list_and_each_set_of_a_one_of() {
        let list: LaunchVariables = serde_json5::from_str(r#"["B_KEY", "A_KEY"]"#).unwrap();
        assert_eq!(list.describe(), "`A_KEY`, `B_KEY`");
        assert_eq!(
            one_of(BRAIN_ONE_OF).describe(),
            "one set of `OPENAI_COMPATIBLE_API_BASE` and `OPENAI_COMPATIBLE_API_KEY`, or of \
             `GEMINI_API_KEY`"
        );
        assert_eq!(
            one_of(r#"{ one_of: [["A_KEY", "B_KEY", "C_KEY"], ["D_KEY"], ["E_KEY"]] }"#).describe(),
            "one set of `A_KEY`, `B_KEY` and `C_KEY`, or of `D_KEY`, or of `E_KEY`"
        );
    }

    /// A name of any set of a `one_of` that `env_vars` also gives is found:
    /// whichever set is taken, no variable gets two values.
    #[test]
    fn a_name_of_any_set_that_env_vars_also_gives_is_found() {
        let env_vars = BTreeMap::from([("GEMINI_API_KEY".to_owned(), "x".to_owned())]);
        assert_eq!(
            names(one_of(BRAIN_ONE_OF).given_in(&env_vars)),
            ["GEMINI_API_KEY"]
        );
    }

    /// An instance with a `one_of` on a peer is refused, and the refusal
    /// names its sets.
    #[test]
    fn an_instance_with_a_one_of_is_refused_on_a_peer() {
        let mut brain = DeploymentInstance::empty(name("brain_inst"));
        brain.env_from_launch = one_of(BRAIN_ONE_OF);
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

    /// The need of an instance that takes the `one_of` of the hosted brain.
    fn choice(instance: &str, starter: Starter) -> ChoiceNeed {
        ChoiceNeed {
            variables: one_of(BRAIN_ONE_OF),
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
    /// `one_of`, which no variable need stands for.
    #[test]
    fn an_instance_with_a_one_of_needs_a_set_and_no_variable() {
        let mut brain = DeploymentInstance::empty(name("brain_inst"));
        brain.env_from_launch = one_of(BRAIN_ONE_OF);
        let mut needs = VariableNeeds::default();
        needs.add_instance("brain_inst", &brain, &Starter::Launch);
        assert_eq!(needs.iter().count(), 0);
        assert_eq!(
            needs.choices().cloned().collect::<Vec<_>>(),
            [choice("brain_inst", Starter::Launch)]
        );
        assert!(!needs.is_empty());
    }

    /// The needs of a `one_of` merge their scopes and features as the needs
    /// of a variable do, and another `one_of` is another need.
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
        other.variables = one_of(r#"{ one_of: [["A_KEY"], ["B_KEY"]] }"#);
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

    /// The needs of the same `one_of` are one group, in the order added.
    #[test]
    fn the_choice_reasons_group_the_needs_by_one_of() {
        let mut needs = VariableNeeds::default();
        needs.add_choice(choice("alpha_brain_inst", Starter::Launch));
        let mut other = choice("tool_inst", Starter::Launch);
        other.variables = one_of(r#"{ one_of: [["A_KEY"], ["B_KEY"]] }"#);
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

    /// A `one_of` passes the check when one set is complete, and fails it
    /// when each set misses a variable.
    #[test]
    fn a_one_of_passes_the_check_with_one_complete_set() {
        let mut needs = VariableNeeds::default();
        needs.add_choice(choice("brain_inst", Starter::Launch));
        for held in [
            &["GEMINI_API_KEY"][..],
            &["OPENAI_COMPATIBLE_API_BASE", "OPENAI_COMPATIBLE_API_KEY"][..],
        ] {
            needs
                .check(VariableChange::Launch, |name| held.contains(&name))
                .unwrap_or_else(|refusal| panic!("{held:?}: {refusal}"));
        }
        for held in [&[][..], &["OPENAI_COMPATIBLE_API_KEY"][..]] {
            needs
                .check(VariableChange::Launch, |name| held.contains(&name))
                .expect_err("no complete set");
        }
    }

    /// The refusal of one `one_of` names every set, each instance that takes
    /// one and what starts it, in the launch and in the join.
    #[test]
    fn the_refusal_of_one_one_of_names_every_set() {
        let mut needs = VariableNeeds::default();
        needs.add_choice(choice("brain_inst", Starter::Launch));
        assert_eq!(
            needs
                .check(VariableChange::Launch, |_| false)
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
                .check(VariableChange::Join, |_| false)
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

    /// A missing variable of a list and a `one_of` with no complete set are
    /// one refusal, one line each.
    #[test]
    fn a_missing_variable_and_an_unmet_one_of_are_one_refusal() {
        let mut needs = VariableNeeds::default();
        needs.add(VariableNeed {
            variable: variable("TOOL_KEY"),
            instance: "tool_inst".to_owned(),
            starter: Starter::Launch,
        });
        needs.add_choice(choice("brain_inst", Starter::Launch));
        assert_eq!(
            needs
                .check(VariableChange::Launch, |_| false)
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
        other.variables = one_of(r#"{ one_of: [["A_KEY"], ["B_KEY"]] }"#);
        two.add_choice(other);
        let refusal = two
            .check(VariableChange::Launch, |_| false)
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
}
