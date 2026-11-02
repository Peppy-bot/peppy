//! The checks of launch variables that a launch and a join run before they
//! change anything: every instance with launch variables runs on the
//! coordinator, and each variable that an instance takes has a value, not
//! empty. A launch reads its own forwarded environment for the instances it
//! starts and for those a join through a scope can start; a join reads its
//! own forwarded environment, then the values the launch kept, for the
//! instances it starts.

use daemon_config::launcher::{
    DeploymentInstance, ForwardedEnv, LaunchValues, Placements, Starter, VariableChange,
    VariableNeed, VariableNeeds, refuse_peer_placements,
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
    needs.extend(scoped);
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

/// One need for each variable that an instance of `instances` takes, each
/// started by `starter`.
fn needs_of<'a>(
    instances: impl IntoIterator<Item = &'a DeploymentInstance>,
    starter: &Starter,
) -> VariableNeeds {
    let mut needs = VariableNeeds::default();
    for instance in instances {
        needs.extend(VariableNeed::each_of(
            instance.instance_id.as_str(),
            instance,
            starter,
        ));
    }
    needs
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
