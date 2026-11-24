//! The checks of launch variables that a launch and a join run before they
//! change anything: every instance with launch variables runs on the
//! coordinator, each variable of the list of an instance has a value, not
//! empty, and each `any_of` of an instance has a set whose every variable
//! has one in one source. A launch reads its own forwarded environment for
//! the instances it starts and for those a join through a scope can start;
//! a join reads its own forwarded environment, then the values the launch
//! kept, for the instances it starts. A launch and a join refuse a set that
//! their own forwarded environment holds in part, and a launch refuses one
//! of each `any_of` of its launcher, as it keeps the values for each later
//! join.

use daemon_config::launcher::{
    ChangeValues, DeploymentInstance, ForwardedEnv, LaunchValues, Placements, Starter,
    VariableChange, VariableNeeds, WrittenLaunchVariables, refuse_kept_sets_held_in_part,
    refuse_peer_placements,
};

/// The launch checks of launch variables over `instances`, the instances
/// of the launch, and `scoped`, the needs of the instances a join through a
/// scope of the launch can start, against `forwarded`, the forwarded
/// environment of the launch. Then the sets of every `any_of` of the
/// launcher, `written` its every `env_from_launch` and `set_env_from_launch`,
/// that the launch would keep in part.
pub(in crate::services::stack) fn check_launch<'a>(
    instances: impl IntoIterator<Item = &'a DeploymentInstance> + Clone,
    placements: &Placements,
    scoped: VariableNeeds,
    forwarded: &ForwardedEnv,
    written: &[WrittenLaunchVariables<'_>],
) -> Result<(), String> {
    if let Some(refusal) =
        refuse_peer_placements(instances.clone(), placements, VariableChange::Launch)
    {
        return Err(refusal);
    }
    let mut needs = needs_of(instances, &Starter::Launch);
    needs.merge(scoped);
    needs
        .check(ChangeValues::launch(forwarded.launch_values()))
        .map_err(|refusal| refusal.to_string())?;
    refuse_kept_sets_held_in_part(written, forwarded.launch_values())
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
        .check(ChangeValues::join(forwarded.launch_values(), kept))
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
    use config::runtime::{CoreNodeName, Name};
    use daemon_config::launcher::hosted_brain::{self, API_KEY, BASE, PROVIDER_KEY};
    use daemon_config::launcher::{LaunchVariable, LaunchVariables, LaunchVariablesKey};
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

    /// The launch checks of `instances` and `scoped` against `environment`,
    /// for a launcher whose one `any_of` is that of the hosted brain, which
    /// an adjustment writes on `brain_inst`.
    fn check_launch_of_the_brain_launcher<'a>(
        instances: impl IntoIterator<Item = &'a DeploymentInstance> + Clone,
        scoped: VariableNeeds,
        environment: &ForwardedEnv,
    ) -> Result<(), String> {
        let any_of = hosted_brain::any_of();
        let written = [WrittenLaunchVariables {
            variables: &any_of,
            instance: "brain_inst",
            key: LaunchVariablesKey::SetEnvFromLaunch,
            document: "simulation_mcp.json5",
        }];
        check_launch(instances, &on_coordinator(), scoped, environment, &written)
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
            &[],
        )
        .expect("the launch environment holds the key");
        for environment in [forwarded(&[("PATH", "/usr/bin")]), forwarded(&[(KEY, "")])] {
            let refusal = check_launch(
                [&planner],
                &on_coordinator(),
                VariableNeeds::default(),
                &environment,
                &[],
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
                &[],
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
        let brain = hosted_brain::brain("brain_inst");
        for environment in [
            hosted_brain::forwarded(&[(BASE, "sentinel-base"), (API_KEY, "sentinel-api-key")]),
            hosted_brain::forwarded(&[(PROVIDER_KEY, "sentinel-provider-key")]),
            hosted_brain::forwarded(&[
                (BASE, "sentinel-base"),
                (API_KEY, "sentinel-api-key"),
                (PROVIDER_KEY, "sentinel-provider-key"),
            ]),
        ] {
            check_launch_of_the_brain_launcher([&brain], VariableNeeds::default(), &environment)
                .unwrap_or_else(|refusal| panic!("{environment:?}: {refusal}"));
        }
        for environment in [
            hosted_brain::forwarded(&[]),
            hosted_brain::forwarded(&[
                (BASE, ""),
                (API_KEY, ""),
                (PROVIDER_KEY, ""),
                ("PATH", "/usr/bin"),
            ]),
        ] {
            let refusal = check_launch_of_the_brain_launcher(
                [&brain],
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
        let brain = hosted_brain::brain("brain_inst");
        for (environment, missing) in [
            (
                hosted_brain::forwarded(&[(API_KEY, "sentinel-api-key"), ("PATH", "/usr/bin")]),
                BASE,
            ),
            (
                hosted_brain::forwarded(&[
                    (BASE, "sentinel-base"),
                    (PROVIDER_KEY, "sentinel-provider-key"),
                ]),
                API_KEY,
            ),
            (
                hosted_brain::forwarded(&[
                    (BASE, "sentinel-base"),
                    (API_KEY, ""),
                    (PROVIDER_KEY, "sentinel-provider-key"),
                ]),
                API_KEY,
            ),
        ] {
            let refusal = check_launch_of_the_brain_launcher(
                [&brain],
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

    /// A launch that starts no instance of the `any_of`, and whose scopes can
    /// start none, is refused when it holds a set of it in part, as it keeps
    /// the values for each later join: the refusal names the set, its
    /// missing variable and where the launcher writes the `any_of`. The
    /// launch passes with a complete set alone, and with no variable of the
    /// `any_of`.
    #[test]
    fn a_launch_refuses_a_set_held_in_part_that_it_would_keep() {
        let no_instance = std::iter::empty::<&DeploymentInstance>();
        for environment in [
            hosted_brain::forwarded(&[]),
            hosted_brain::forwarded(&[(PROVIDER_KEY, "sentinel")]),
            hosted_brain::forwarded(&[(BASE, "sentinel"), (API_KEY, "sentinel")]),
        ] {
            check_launch_of_the_brain_launcher(
                no_instance.clone(),
                VariableNeeds::default(),
                &environment,
            )
            .unwrap_or_else(|refusal| panic!("{environment:?}: {refusal}"));
        }
        let refusal = check_launch_of_the_brain_launcher(
            no_instance,
            VariableNeeds::default(),
            &hosted_brain::forwarded(&[(PROVIDER_KEY, "sentinel"), (API_KEY, "sentinel")]),
        )
        .expect_err("the set of the API held in part");
        assert_eq!(
            refusal,
            "the launch environment holds the set of `OPENAI_COMPATIBLE_API_BASE` and \
             `OPENAI_COMPATIBLE_API_KEY` in part, as it does not hold \
             `OPENAI_COMPATIBLE_API_BASE`, or holds it empty or with characters that a node \
             cannot receive: the `set_env_from_launch` of `brain_inst` in simulation_mcp.json5 \
             names it in one set of `OPENAI_COMPATIBLE_API_BASE` and \
             `OPENAI_COMPATIBLE_API_KEY`, or of `GEMINI_API_KEY`, and the launch keeps the value \
             of each launch variable of its launcher for the joins. A launch and a join refuse a \
             set that their own environment holds in part, also when another set is complete, \
             so that an instance never takes another set in its place: export every variable \
             of such a set, or none of them, in the shell that launches. A value must not be \
             empty, and must hold letters, digits and `_ - . / : , = @ + %` only"
        );
    }

    /// A join through a scope takes a set that the launch kept, and a CLI
    /// join a complete set of its own environment, also when the kept values
    /// hold another set complete.
    #[test]
    fn a_join_takes_a_complete_set_of_its_own_environment_or_of_the_kept_values() {
        let joined = hosted_brain::brain("alpha_brain_inst");
        let kept_provider_key = hosted_brain::values(&[(PROVIDER_KEY, "kept")]);
        let kept_api = hosted_brain::values(&[(BASE, "kept"), (API_KEY, "kept")]);
        for (environment, kept_values) in [
            (hosted_brain::forwarded(&[]), &kept_provider_key),
            (hosted_brain::forwarded(&[]), &kept_api),
            (
                hosted_brain::forwarded(&[(BASE, "joined"), (API_KEY, "joined")]),
                &kept_provider_key,
            ),
            (
                hosted_brain::forwarded(&[(PROVIDER_KEY, "joined")]),
                &kept_api,
            ),
        ] {
            check_join([&joined], &on_coordinator(), &environment, kept_values)
                .unwrap_or_else(|refusal| panic!("{environment:?}: {refusal}"));
        }
    }

    /// A join whose own environment holds a set in part is refused, also
    /// when the kept values hold that set complete or hold the rest of it: a
    /// set takes its values from one source, so a key of the join shell
    /// never reaches the base address that the launch kept, and a base
    /// address of the join shell never receives the kept key. The refusal
    /// names the missing variable and says to export every variable of the
    /// set, or none of them, in the shell that joins.
    #[test]
    fn a_join_whose_own_environment_holds_a_set_in_part_is_refused() {
        let joined = hosted_brain::brain("alpha_brain_inst");
        let kept_api = hosted_brain::values(&[(BASE, "sentinel-kept"), (API_KEY, "sentinel-kept")]);
        for (environment, kept_values, missing) in [
            (
                hosted_brain::forwarded(&[(API_KEY, "sentinel-joined")]),
                LaunchValues::default(),
                BASE,
            ),
            (
                hosted_brain::forwarded(&[(API_KEY, "sentinel-joined")]),
                hosted_brain::values(&[(BASE, "sentinel-kept")]),
                BASE,
            ),
            (
                hosted_brain::forwarded(&[(API_KEY, "sentinel-joined")]),
                kept_api.clone(),
                BASE,
            ),
            (
                hosted_brain::forwarded(&[(BASE, "sentinel-joined")]),
                kept_api,
                API_KEY,
            ),
            (
                hosted_brain::forwarded(&[(BASE, "sentinel-joined"), (PROVIDER_KEY, "sentinel")]),
                LaunchValues::default(),
                API_KEY,
            ),
        ] {
            let refusal = check_join([&joined], &on_coordinator(), &environment, &kept_values)
                .expect_err("a set held in part");
            assert!(
                refusal.starts_with("the join needs one set of `OPENAI_COMPATIBLE_API_BASE`")
                    && refusal.contains(&format!(
                        "the join environment holds the set of `OPENAI_COMPATIBLE_API_BASE` and \
                         `OPENAI_COMPATIBLE_API_KEY` in part, as it does not hold `{missing}`"
                    ))
                    && refusal.contains(
                        "`alpha_brain_inst` takes the first complete set from the launch, and \
                         the join starts it"
                    )
                    && refusal.contains(
                        "export every variable of such a set, or none of them, in the shell \
                         that joins"
                    )
                    && !refusal.contains("sentinel"),
                "{environment:?}: {refusal}"
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
            &hosted_brain::brain("brain_inst"),
            &Starter::ScopedJoin {
                option: Name::new("demo_arm").unwrap(),
                features: vec![],
                scopes: vec!["framework_controls_inst".to_owned()],
            },
        );
        let no_instance = std::iter::empty::<&DeploymentInstance>();
        check_launch_of_the_brain_launcher(
            no_instance.clone(),
            scoped.clone(),
            &hosted_brain::forwarded(&[(PROVIDER_KEY, "sentinel")]),
        )
        .expect("the launch holds a complete set");
        for (environment, expected) in [
            (hosted_brain::forwarded(&[]), "holds no set complete"),
            (
                hosted_brain::forwarded(&[(BASE, "sentinel"), (PROVIDER_KEY, "sentinel")]),
                "in part, as it does not hold `OPENAI_COMPATIBLE_API_KEY`",
            ),
        ] {
            let refusal = check_launch_of_the_brain_launcher(
                no_instance.clone(),
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
