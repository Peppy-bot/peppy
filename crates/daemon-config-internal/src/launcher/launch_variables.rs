//! Launch variables: the environment variables that an instance takes from
//! the environment of the launch, by name. A launcher names them in the
//! `env_from_launch` of an instance and with a `set_env_from_launch`
//! adjustment, so no launcher file holds a value.
//!
//! The daemon keeps, in memory, the value of each name of the launcher that
//! the forwarded environment of the launch holds ([`LaunchValues`]), and a
//! join gives it to each instance that it starts and that names it. The
//! launch is refused when an instance that the launch or a join through a
//! `robot_membership` scope can start takes a variable that the forwarded
//! environment of the launch does not hold, and a join when an instance
//! that it starts takes a variable that neither its own forwarded
//! environment nor the kept values hold ([`VariableNeeds::check`]). An
//! instance with launch variables runs on the coordinator's machine only
//! ([`refuse_peer_placements`]).

use super::types::{DeploymentInstance, Placements};
use crate::internal::env::{InvalidEnvVar, SAFE_ENV_VALUE_CHARACTERS, check_launch_variable_name};
use config::runtime::Name;
use serde::{Deserialize, Serialize};
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
/// once, in the order written. An empty list is the same as no list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<LaunchVariable>", into = "Vec<LaunchVariable>")]
pub struct LaunchVariables(Vec<LaunchVariable>);

impl TryFrom<Vec<LaunchVariable>> for LaunchVariables {
    type Error = RepeatedLaunchVariable;

    fn try_from(variables: Vec<LaunchVariable>) -> Result<Self, Self::Error> {
        let mut seen = BTreeSet::new();
        if let Some(repeated) = variables.iter().find(|variable| !seen.insert(*variable)) {
            return Err(RepeatedLaunchVariable {
                variable: repeated.clone(),
            });
        }
        Ok(Self(variables))
    }
}

impl From<LaunchVariables> for Vec<LaunchVariable> {
    fn from(variables: LaunchVariables) -> Self {
        variables.0
    }
}

impl LaunchVariables {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &LaunchVariable> {
        self.0.iter()
    }

    /// The first variable of the list that `env_vars` also gives a value:
    /// that variable would get two values.
    pub fn given_in(&self, env_vars: &BTreeMap<String, String>) -> Option<&LaunchVariable> {
        self.0
            .iter()
            .find(|variable| env_vars.contains_key(variable.as_str()))
    }
}

/// The refusal of an instance that takes `variable` from the launch and also
/// gives it a value in its `env_vars`. `list_origin` names the document or the
/// adjustment that wrote the list of the instance, when the composition knows
/// it.
pub(crate) fn given_two_values(
    instance: &str,
    variable: &impl std::fmt::Display,
    list_origin: Option<&str>,
) -> String {
    let list = match list_origin {
        Some(origin) => format!("`env_from_launch` (set by {origin})"),
        None => String::from("`env_from_launch`"),
    };
    format!(
        "instance `{instance}` takes `{variable}` from the launch in {list} and gives it a value \
         in `env_vars`, so the variable would get two values; take it out of one of the two"
    )
}

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
        instance
            .env_from_launch
            .iter()
            .map(move |variable| VariableNeed {
                variable: variable.clone(),
                instance: shown.to_owned(),
                starter: starter.clone(),
            })
    }

    fn render(&self) -> String {
        let instance = &self.instance;
        match &self.starter {
            Starter::Launch => {
                format!("`{instance}` takes it from the launch, and the launch starts it")
            }
            Starter::Join => {
                format!("`{instance}` takes it from the launch, and the join starts it")
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
                format!(
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

    /// The refusal of `change` when a variable of these needs is one that
    /// `holds` does not hold: it names each such variable, each instance that
    /// needs it and what can start that instance.
    pub fn check(
        &self,
        change: VariableChange,
        holds: impl Fn(&str) -> bool,
    ) -> Result<(), MissingLaunchVariables> {
        let mut missing: BTreeMap<LaunchVariable, Vec<VariableNeed>> = BTreeMap::new();
        for need in self.0.iter().filter(|need| !holds(need.variable.as_str())) {
            missing
                .entry(need.variable.clone())
                .or_default()
                .push(need.clone());
        }
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

/// The change that needs launch variables, as its refusal names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariableChange {
    Launch,
    Join,
}

/// A launch or a join refused because one of its instances takes a launch
/// variable that its environment does not hold. The CLI does not forward a
/// value with characters that a node cannot receive, so the refusal also
/// names that cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingLaunchVariables {
    change: VariableChange,
    missing: BTreeMap<LaunchVariable, Vec<VariableNeed>>,
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
        let (pronoun, value, values) = if one {
            ("it", "a value", "a value")
        } else {
            ("them", "values", "values")
        };
        let (change, environment, shell) = match self.change {
            VariableChange::Launch => (
                "the launch",
                format!("the launch environment does not hold {pronoun}, or holds {value}"),
                "the shell that launches",
            ),
            VariableChange::Join => (
                "the join",
                format!(
                    "neither the join environment nor the launch environment holds {pronoun}, or \
                     they hold {value}"
                ),
                "the shell that joins or in the shell that launches",
            ),
        };
        let reasons = |needs: &[VariableNeed]| {
            needs
                .iter()
                .map(VariableNeed::render)
                .collect::<Vec<_>>()
                .join("; ")
        };
        let details = if one {
            format!(" {}", reasons(&self.missing[variables[0]]))
        } else {
            crate::error::format_bulleted(
                self.missing
                    .iter()
                    .map(|(variable, needs)| format!("`{variable}`: {}", reasons(needs))),
            )
        };
        let export = if one {
            format!("`{}`", variables[0])
        } else {
            String::from("each of them")
        };
        let separator = if one { " " } else { "\n" };
        write!(
            f,
            "{change} needs {}, and {environment} with characters that a node cannot \
             receive:{details}.{separator}Export {export} in {shell}, with {values} of \
             {SAFE_ENV_VALUE_CHARACTERS} only",
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
    let change = match change {
        VariableChange::Launch => "the launch",
        VariableChange::Join => "the join",
    };
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

/// The values that the daemon keeps of the launch variables of the stack it
/// coordinates: the value of each name of its launcher that the forwarded
/// environment of the launch holds. They live in memory with the stack, and
/// the debug output names the variables only.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct LaunchValues(BTreeMap<LaunchVariable, String>);

impl LaunchValues {
    /// The value of each of `names` that `forwarded` holds.
    pub fn kept<'a>(
        names: impl IntoIterator<Item = &'a LaunchVariable>,
        forwarded: &[(String, String)],
    ) -> Self {
        Self(
            names
                .into_iter()
                .filter_map(|name| {
                    forwarded
                        .iter()
                        .find(|(key, _)| key == name.as_str())
                        .map(|(_, value)| (name.clone(), value.clone()))
                })
                .collect(),
        )
    }

    /// The kept value of the variable `name`.
    pub fn value_of(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(variable, _)| variable.as_str() == name)
            .map(|(_, value)| value.as_str())
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
    /// name written two times, and writes back as it was written.
    #[test]
    fn a_list_parses_each_name_once() {
        let parsed: LaunchVariables =
            serde_json5::from_str(r#"["GEMINI_API_KEY", "OPENROUTER_API_KEY"]"#).unwrap();
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
            (r#"["A", "A"]"#, "names `A` two times"),
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
             holds a value with characters that a node cannot receive: `brain_inst` of option \
             `demo_arm` takes it from the launch, and stack.join through \
             `framework_controls_inst` can add a copy of `demo_arm`; `planner_inst` takes it \
             from the launch, and the launch starts it. Export `GEMINI_API_KEY` in the shell \
             that launches, with a value of letters, digits and `_ - . / : , = @ + %` only"
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
             environment holds them, or they hold values with characters that a node cannot \
             receive:\n  - `A_KEY`: `brain_inst` of option `demo_arm` takes it from the launch, \
             and stack.join through `a_inst` can add a copy of `demo_arm` with the features \
             `router`, `camera`\n  - `B_KEY`: `brain_inst` of option `demo_arm` takes it from \
             the launch, and stack.join through `a_inst` or `b_inst` can add a copy of \
             `demo_arm` with the feature `router`.\nExport each of them in the shell that joins \
             or in the shell that launches, with values of letters, digits and \
             `_ - . / : , = @ + %` only"
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

    /// The kept values are those of the launcher's names that the forwarded
    /// environment holds, and their debug output names the variables only.
    #[test]
    fn kept_values_hold_the_named_variables_and_print_no_value() {
        let names = [variable("GEMINI_API_KEY"), variable("ABSENT_KEY")];
        let kept = LaunchValues::kept(
            &names,
            &[
                ("GEMINI_API_KEY".to_owned(), "sentinel-value".to_owned()),
                ("PATH".to_owned(), "/usr/bin".to_owned()),
            ],
        );
        assert_eq!(kept.value_of("GEMINI_API_KEY"), Some("sentinel-value"));
        assert_eq!(kept.value_of("ABSENT_KEY"), None);
        assert_eq!(kept.value_of("PATH"), None);
        assert_eq!(format!("{kept:?}"), r#"LaunchValues(["GEMINI_API_KEY"])"#);
    }
}
