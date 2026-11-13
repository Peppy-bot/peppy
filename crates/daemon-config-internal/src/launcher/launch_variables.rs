//! Launch variables: the environment variables that an instance takes from
//! the environment of the launch, by name. A launcher names them in the
//! `env_from_launch` of an instance and with a `set_env_from_launch`
//! adjustment, so no launcher file holds a value.
//!
//! A launch and a join parse the environment that the CLI forwarded with
//! them against the launch variables of the launcher ([`ForwardedEnv`]): the
//! value of a launch variable reaches the run of each instance that names it,
//! and no other node, add or build. The daemon keeps, in memory, the values
//! of the launch ([`LaunchValues`]), and a join gives each instance that it
//! starts the kept value of each variable that the instance names and the
//! join does not forward. The launch is refused when an instance that the
//! launch or a join through a `robot_membership` scope can start takes a
//! variable that the forwarded environment of the launch does not hold, and a
//! join when an instance that it starts takes a variable that neither its own
//! forwarded environment nor the kept values hold ([`VariableNeeds::check`]).
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

/// The variables that one instance takes from the launch environment, each
/// once. They are a set: two lists that name the same variables in another
/// order are the same, and a list writes back in name order. An empty list
/// is the same as no list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<LaunchVariable>", into = "Vec<LaunchVariable>")]
pub struct LaunchVariables(BTreeSet<LaunchVariable>);

impl TryFrom<Vec<LaunchVariable>> for LaunchVariables {
    type Error = RepeatedLaunchVariable;

    fn try_from(written: Vec<LaunchVariable>) -> Result<Self, Self::Error> {
        let mut variables = BTreeSet::new();
        for variable in written {
            if variables.contains(&variable) {
                return Err(RepeatedLaunchVariable { variable });
            }
            variables.insert(variable);
        }
        Ok(Self(variables))
    }
}

impl From<LaunchVariables> for Vec<LaunchVariable> {
    fn from(variables: LaunchVariables) -> Self {
        variables.0.into_iter().collect()
    }
}

impl LaunchVariables {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The variables, in name order.
    pub fn iter(&self) -> impl Iterator<Item = &LaunchVariable> {
        self.0.iter()
    }

    /// The variables of the list that `env_vars` also gives a value, in name
    /// order: each one would get two values.
    pub fn given_in<'a>(
        &'a self,
        env_vars: &'a BTreeMap<String, String>,
    ) -> impl Iterator<Item = &'a LaunchVariable> + 'a {
        self.0
            .iter()
            .filter(|variable| env_vars.contains_key(variable.as_str()))
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
    /// One need for each variable that `instance` takes, `instance` named
    /// `shown`.
    pub fn each_of<'a>(
        shown: &'a str,
        instance: &'a DeploymentInstance,
        starter: &'a Starter,
    ) -> impl Iterator<Item = Self> + 'a {
        instance.env_from_launch.iter().map(move |variable| Self {
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
        let instance = &self.instance;
        match &self.starter {
            Starter::Launch => {
                write!(
                    f,
                    "`{instance}` takes it from the launch, and the launch starts it"
                )
            }
            Starter::Join => {
                write!(
                    f,
                    "`{instance}` takes it from the launch, and the join starts it"
                )
            }
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
                    "`{instance}` of option `{option}` takes it from the launch, and stack.join \
                     through {} can add a copy of `{option}`{with}",
                    quoted_series(scopes, "or")
                )
            }
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

/// The instances that take launch variables, each with what can start it,
/// one entry per variable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VariableNeeds(Vec<VariableNeed>);

impl VariableNeeds {
    /// Adds `need`. A need that an entry already gives is left out: an entry
    /// of the same variable, instance and option, through a scope of each
    /// scope of `need`, with features that `need` holds. An entry that differs
    /// from `need` by its scopes alone takes the scopes of `need`.
    pub fn add(&mut self, need: VariableNeed) {
        let Starter::ScopedJoin {
            option,
            features,
            scopes,
        } = &need.starter
        else {
            if !self.0.contains(&need) {
                self.0.push(need);
            }
            return;
        };
        for held in &mut self.0 {
            let Starter::ScopedJoin {
                option: held_option,
                features: held_features,
                scopes: held_scopes,
            } = &mut held.starter
            else {
                continue;
            };
            if held.variable != need.variable
                || held.instance != need.instance
                || held_option != option
            {
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
        self.0.push(need);
    }

    /// Adds each need of `other`.
    pub fn extend(&mut self, other: impl IntoIterator<Item = VariableNeed>) {
        for need in other {
            self.add(need);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &VariableNeed> {
        self.0.iter()
    }

    /// Each variable of these needs, in name order, with each instance that
    /// takes it and what can start that instance, as one clause.
    pub fn reasons(&self) -> BTreeMap<LaunchVariable, String> {
        let mut grouped: BTreeMap<LaunchVariable, Vec<String>> = BTreeMap::new();
        for need in &self.0 {
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

    /// The refusal of `change` when a variable of these needs is one that
    /// `holds` does not hold: it names each such variable, each instance that
    /// needs it and what can start that instance.
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
        if missing.is_empty() {
            Ok(())
        } else {
            Err(MissingLaunchVariables { change, missing })
        }
    }
}

impl IntoIterator for VariableNeeds {
    type Item = VariableNeed;
    type IntoIter = std::vec::IntoIter<VariableNeed>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
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
/// variable that its environment does not hold. The CLI does not forward a
/// value with characters that a node cannot receive, and the daemon counts
/// an empty value as no value, so the refusal also names these causes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingLaunchVariables {
    change: VariableChange,
    /// Each missing variable, with what needs it ([`VariableNeeds::reasons`]).
    missing: BTreeMap<LaunchVariable, String>,
}

impl MissingLaunchVariables {
    /// The variables the change needs and does not get, in name order.
    pub fn variables(&self) -> impl Iterator<Item = &LaunchVariable> {
        self.missing.keys()
    }
}

impl std::fmt::Display for MissingLaunchVariables {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let variables: Vec<&LaunchVariable> = self.missing.keys().collect();
        let one = variables.len() == 1;
        let them = if one { "it" } else { "them" };
        let (environment, shell) = match self.change {
            VariableChange::Launch => (
                format!("the launch environment does not hold {them}, or holds {them}"),
                "the shell that launches",
            ),
            VariableChange::Join => (
                format!(
                    "neither the join environment nor the launch environment holds {them}, or \
                     they hold {them}"
                ),
                "the shell that joins or in the shell that launches",
            ),
        };
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
                    crate::error::format_quoted_list(instance.env_from_launch.iter())
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
        assert!(VariablesGivenTwoValues::refuse(Vec::new()).is_ok());
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
        assert!(needs.check(VariableChange::Launch, |_| true).is_ok());
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
}
