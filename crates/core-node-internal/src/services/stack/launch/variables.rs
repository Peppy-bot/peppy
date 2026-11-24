//! The checks of launch variables that a launch and a join run before they
//! change anything: every instance with launch variables runs on the
//! coordinator, each variable of the list of an instance has a value, not
//! empty, and each `any_of` of an instance has a set whose every variable
//! has one, and no set of which some variables have one and others not. A
//! launch reads its own forwarded environment for the instances
//! it starts and for those a join through a scope can start; a join reads
//! its own forwarded environment, then the values the launch kept, for the
//! instances it starts.

use daemon_config::launcher::{
    DeploymentInstance, ForwardedEnv, LaunchValues, Placements, Starter, VariableChange,
    VariableNeeds, refuse_peer_placements,
};

/// The launch checks of launch variables over `instances`, the instances
/// of the launch, and `scoped`, the needs of the instances a join through a
/// scope of the launch can start, against `forwarded`, the forwarded
/// environment of the launch.
pub(in crate::services::stack) fn check_launch<'a>(
    instances: impl IntoIterator<Item = &'a DeploymentInstance> + Clone,
    placements: &Placements,
    scoped: VariableNeeds,
    forwarded: &ForwardedEnv,
) -> Result<(), String> {
    if let Some(refusal) =
        refuse_peer_placements(instances.clone(), placements, VariableChange::Launch)
    {
        return Err(refusal);
    }
    let mut needs = needs_of(instances, &Starter::Launch);
    needs.merge(scoped);
    needs
        .check(VariableChange::Launch, |name| {
            forwarded.launch_values().holds(name)
        })
        .map_err(|refusal| refusal.to_string())
}

/// The join checks of launch variables over `instances`, the instances the
/// join starts, against `forwarded`, the forwarded environment of the join,
/// then `kept`, the values the launch kept.
pub(in crate::services::stack) fn check_join<'a>(
    instances: impl IntoIterator<Item = &'a DeploymentInstance> + Clone,
    placements: &Placements,
    forwarded: &ForwardedEnv,
    kept: &LaunchValues,
) -> Result<(), String> {
    if let Some(refusal) =
        refuse_peer_placements(instances.clone(), placements, VariableChange::Join)
    {
        return Err(refusal);
    }
    needs_of(instances, &Starter::Join)
        .check(VariableChange::Join, |name| {
            forwarded.launch_values().holds(name) || kept.holds(name)
        })
        .map_err(|refusal| refusal.to_string())
}

/// The needs of each instance of `instances` that takes launch variables,
/// one for each variable of its list or one for its `any_of`, each started
/// by `starter`.
fn needs_of<'a>(
    instances: impl IntoIterator<Item = &'a DeploymentInstance>,
    starter: &Starter,
) -> VariableNeeds {
    let mut needs = VariableNeeds::default();
    for instance in instances {
        needs.add_instance(instance.instance_id.as_str(), instance, starter);
    }
    needs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::stack::launch::any_of_fixtures::{
        API_KEY, BASE, PROVIDER_KEY, brain_with_any_of, forwarded_to_the_brain,
    };
    use config::runtime::{CoreNodeName, Name};
    use daemon_config::launcher::{LaunchVariable, LaunchVariables};
    use std::collections::{BTreeMap, BTreeSet};

    const KEY: &str = "GEMINI_API_KEY";

    fn key() -> LaunchVariable {
        LaunchVariable::try_from(KEY.to_owned()).unwrap()
    }

    fn brain(id: &str) -> DeploymentInstance {
        let mut instance = DeploymentInstance::empty(Name::new(id).unwrap());
        instance.env_from_launch = LaunchVariables::try_from(vec![key()]).unwrap();
        instance
    }

    fn on_coordinator() -> Placements {
        Placements::all_on(CoreNodeName::new("coordinator").unwrap())
    }

    /// `entries` as the forwarded environment of a launcher whose launch
    /// variable is `GEMINI_API_KEY`.
    fn forwarded(entries: &[(&str, &str)]) -> ForwardedEnv {
        ForwardedEnv::parse(
            entries
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
            &BTreeSet::from([key()]),
        )
    }

    /// The values a launch whose forwarded environment is `entries` keeps.
    fn kept(entries: &[(&str, &str)]) -> LaunchValues {
        forwarded(entries).launch_values().clone()
    }

    /// A launch whose forwarded environment holds the variable of each
    /// instance it starts passes; without it, or with an empty value, the
    /// refusal names the instance that the launch starts.
    #[test]
    fn a_launch_needs_the_variables_of_the_instances_it_starts() {
        let planner = brain("planner_inst");
        check_launch(
            [&planner],
            &on_coordinator(),
            VariableNeeds::default(),
            &forwarded(&[(KEY, "sentinel")]),
        )
        .expect("the launch environment holds the key");
        for environment in [forwarded(&[("PATH", "/usr/bin")]), forwarded(&[(KEY, "")])] {
            let refusal = check_launch(
                [&planner],
                &on_coordinator(),
                VariableNeeds::default(),
                &environment,
            )
            .expect_err("no key");
            assert!(
                refusal
                    .contains("`planner_inst` takes it from the launch, and the launch starts it"),
                "{refusal}"
            );
        }
    }

    /// A join takes the variable from its own forwarded environment or the
    /// kept values, and is refused when neither holds it: an empty value of
    /// the join is no value.
    #[test]
    fn a_join_reads_its_own_environment_then_the_kept_values() {
        let joined = brain("alpha_brain_inst");
        let no_value = LaunchValues::default();
        check_join(
            [&joined],
            &on_coordinator(),
            &forwarded(&[(KEY, "")]),
            &kept(&[(KEY, "kept")]),
        )
        .expect("the launch kept the key");
        check_join(
            [&joined],
            &on_coordinator(),
            &forwarded(&[(KEY, "joined")]),
            &no_value,
        )
        .expect("the join forwards the key");
        for environment in [forwarded(&[]), forwarded(&[(KEY, "")])] {
            let refusal = check_join([&joined], &on_coordinator(), &environment, &no_value)
                .expect_err("nothing holds the key");
            assert!(
                refusal.starts_with("the join needs `GEMINI_API_KEY`")
                    && refusal.contains(
                        "`alpha_brain_inst` takes it from the launch, and the join starts it"
                    ),
                "{refusal}"
            );
        }
    }

    /// An instance with launch variables on a peer is refused at a launch
    /// and at a join, whatever the environment holds.
    #[test]
    fn an_instance_with_launch_variables_on_a_peer_is_refused() {
        let joined = brain("alpha_brain_inst");
        let placements = Placements::new(
            CoreNodeName::new("coordinator").unwrap(),
            BTreeMap::from([(
                "alpha_brain_inst".to_owned(),
                CoreNodeName::new("peer_b").unwrap(),
            )]),
        );
        let environment = forwarded(&[(KEY, "sentinel")]);
        for refusal in [
            check_launch(
                [&joined],
                &placements,
                VariableNeeds::default(),
                &environment,
            ),
            check_join(
                [&joined],
                &placements,
                &environment,
                &LaunchValues::default(),
            ),
        ] {
            let refusal = refusal.expect_err("the brain runs on a peer");
            assert!(
                refusal.contains("`alpha_brain_inst` takes `GEMINI_API_KEY` from the launch")
                    && refusal.contains("`peer_b`"),
                "{refusal}"
            );
        }
    }

    /// A launch passes with the first set, the second set or both sets
    /// complete. It is refused, naming every set, when it holds no variable
    /// of any set: an empty value is no value. No value that the environment
    /// holds is in a refusal.
    #[test]
    fn a_launch_needs_one_complete_set_of_an_any_of() {
        let brain = brain_with_any_of("brain_inst");
        for environment in [
            forwarded_to_the_brain(&[(BASE, "sentinel-base"), (API_KEY, "sentinel-api-key")]),
            forwarded_to_the_brain(&[(PROVIDER_KEY, "sentinel-provider-key")]),
            forwarded_to_the_brain(&[
                (BASE, "sentinel-base"),
                (API_KEY, "sentinel-api-key"),
                (PROVIDER_KEY, "sentinel-provider-key"),
            ]),
        ] {
            check_launch(
                [&brain],
                &on_coordinator(),
                VariableNeeds::default(),
                &environment,
            )
            .unwrap_or_else(|refusal| panic!("{environment:?}: {refusal}"));
        }
        for environment in [
            forwarded_to_the_brain(&[]),
            forwarded_to_the_brain(&[
                (BASE, ""),
                (API_KEY, ""),
                (PROVIDER_KEY, ""),
                ("PATH", "/usr/bin"),
            ]),
        ] {
            let refusal = check_launch(
                [&brain],
                &on_coordinator(),
                VariableNeeds::default(),
                &environment,
            )
            .expect_err("no complete set");
            assert_eq!(
                refusal,
                "the launch needs one set of `OPENAI_COMPATIBLE_API_BASE` and \
                 `OPENAI_COMPATIBLE_API_KEY`, or of `GEMINI_API_KEY`, and the launch environment \
                 holds no set complete, or holds a variable of it empty or with characters that \
                 a node cannot receive: `brain_inst` takes the first complete set from the \
                 launch, and the launch starts it. Export every variable of one set in the shell \
                 that launches. A value must not be empty, and must hold letters, digits and \
                 `_ - . / : , = @ + %` only"
            );
        }
    }

    /// A launch that holds a set in part is refused, naming its missing
    /// variable, alone and beside a complete set: a set whose variable is
    /// empty is held in part too.
    #[test]
    fn a_launch_that_holds_a_set_in_part_is_refused() {
        let brain = brain_with_any_of("brain_inst");
        for (environment, missing) in [
            (
                forwarded_to_the_brain(&[(API_KEY, "sentinel-api-key"), ("PATH", "/usr/bin")]),
                BASE,
            ),
            (
                forwarded_to_the_brain(&[
                    (BASE, "sentinel-base"),
                    (PROVIDER_KEY, "sentinel-provider-key"),
                ]),
                API_KEY,
            ),
            (
                forwarded_to_the_brain(&[
                    (BASE, "sentinel-base"),
                    (API_KEY, ""),
                    (PROVIDER_KEY, "sentinel-provider-key"),
                ]),
                API_KEY,
            ),
        ] {
            let refusal = check_launch(
                [&brain],
                &on_coordinator(),
                VariableNeeds::default(),
                &environment,
            )
            .expect_err("a set held in part");
            assert!(
                refusal.contains(&format!(
                    "the launch environment holds the set of `OPENAI_COMPATIBLE_API_BASE` and \
                     `OPENAI_COMPATIBLE_API_KEY` in part, as it does not hold `{missing}`"
                )) && refusal.contains(
                    "`brain_inst` takes the first complete set from the launch, and the launch \
                     starts it"
                ) && !refusal.contains("sentinel"),
                "{environment:?}: {refusal}"
            );
        }
    }

    /// A join through a scope takes the set that the launch kept, a CLI join
    /// completes a set from its own environment first, then the kept values,
    /// and a join that neither completes, or that they hold in part, is
    /// refused.
    #[test]
    fn a_join_completes_a_set_from_its_own_environment_then_the_kept_values() {
        let joined = brain_with_any_of("alpha_brain_inst");
        let kept_of =
            |entries: &[(&str, &str)]| forwarded_to_the_brain(entries).launch_values().clone();
        let kept_provider_key = kept_of(&[(PROVIDER_KEY, "kept")]);
        for (environment, kept_values) in [
            (forwarded_to_the_brain(&[]), &kept_provider_key),
            (
                forwarded_to_the_brain(&[(BASE, "joined"), (API_KEY, "joined")]),
                &kept_provider_key,
            ),
            (
                forwarded_to_the_brain(&[(API_KEY, "joined")]),
                &kept_of(&[(BASE, "kept")]),
            ),
        ] {
            check_join([&joined], &on_coordinator(), &environment, kept_values)
                .unwrap_or_else(|refusal| panic!("{environment:?}: {refusal}"));
        }
        for (environment, kept_values, missing) in [
            (
                forwarded_to_the_brain(&[(API_KEY, "sentinel-joined")]),
                LaunchValues::default(),
                BASE,
            ),
            (
                forwarded_to_the_brain(&[]),
                kept_of(&[(BASE, "sentinel-kept"), (PROVIDER_KEY, "sentinel-kept")]),
                API_KEY,
            ),
        ] {
            let refusal = check_join([&joined], &on_coordinator(), &environment, &kept_values)
                .expect_err("a set held in part");
            assert!(
                refusal.starts_with("the join needs one set of `OPENAI_COMPATIBLE_API_BASE`")
                    && refusal.contains(&format!(
                        "the join environment and the launch environment hold the set of \
                         `OPENAI_COMPATIBLE_API_BASE` and `OPENAI_COMPATIBLE_API_KEY` in part, \
                         as neither holds `{missing}`"
                    ))
                    && refusal.contains(
                        "`alpha_brain_inst` takes the first complete set from the launch, and \
                         the join starts it"
                    )
                    && !refusal.contains("sentinel"),
                "{refusal}"
            );
        }
    }

    /// A launch checks the `any_of` of an instance that a join through its
    /// scope can start, as it checks a variable of a list: it needs a
    /// complete set, and no set held in part.
    #[test]
    fn a_launch_needs_a_complete_set_for_a_brain_that_a_scope_can_add() {
        let mut scoped = VariableNeeds::default();
        scoped.add_instance(
            "brain_inst",
            &brain_with_any_of("brain_inst"),
            &Starter::ScopedJoin {
                option: Name::new("demo_arm").unwrap(),
                features: vec![],
                scopes: vec!["framework_controls_inst".to_owned()],
            },
        );
        check_launch(
            std::iter::empty::<&DeploymentInstance>(),
            &on_coordinator(),
            scoped.clone(),
            &forwarded_to_the_brain(&[(PROVIDER_KEY, "sentinel")]),
        )
        .expect("the launch holds a complete set");
        for (environment, expected) in [
            (forwarded_to_the_brain(&[]), "holds no set complete"),
            (
                forwarded_to_the_brain(&[(BASE, "sentinel"), (PROVIDER_KEY, "sentinel")]),
                "in part, as it does not hold `OPENAI_COMPATIBLE_API_KEY`",
            ),
        ] {
            let refusal = check_launch(
                std::iter::empty::<&DeploymentInstance>(),
                &on_coordinator(),
                scoped.clone(),
                &environment,
            )
            .expect_err("no complete set, or a set held in part");
            assert!(
                refusal.starts_with("the launch needs one set of")
                    && refusal.contains(expected)
                    && refusal.contains(
                        "`brain_inst` of option `demo_arm` takes the first complete set from the \
                         launch, and stack.join through `framework_controls_inst` can add a copy \
                         of `demo_arm`"
                    ),
                "{refusal}"
            );
        }
    }
}
