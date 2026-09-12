//! Composing one join against the running stack: the flat launcher with the
//! copy in it, every deployment planned, and which instances are new. A
//! source the launch already resolved keeps the node and pins the launch
//! ran, so only what the copy brings is resolved here.

use super::super::ChangeResult;
use super::super::action::StackChangeContext;
use super::super::launch::PlannedDeployment;
use super::super::launch::resolve::{self, resolve_deployments};
use super::super::state::ActiveLaunch;
use super::reason;
use config::runtime::{CoreNodeName, Name};
use core_node_api::encoding::StackJoinGoal;
use daemon_config::launcher::{CopyRecord, JoinRequest, PeppyLauncher, Placements, RunningStack};
use std::collections::HashSet;

/// A join composed and planned: the stack's flat document with the copy,
/// every deployment planned, and what is new.
pub(super) struct ResolvedJoin {
    pub combined: PeppyLauncher,
    pub planned: Vec<PlannedDeployment>,
    pub placements: Placements,
    pub new_ids: HashSet<String>,
    pub host: CoreNodeName,
    pub copy: CopyRecord,
    /// The sources this join resolved for the first time, as the launch's
    /// record keeps them.
    pub resolved: Vec<PlannedDeployment>,
}

impl ResolvedJoin {
    pub(super) async fn resolve(
        ctx: &StackChangeContext,
        active: &ActiveLaunch,
        goal: &StackJoinGoal,
    ) -> ChangeResult<Self> {
        active.check_name(&goal.name)?;
        let coordinator = CoreNodeName::new(&ctx.bound_core_node).map_err(|e| e.to_string())?;
        let host = goal.placement.resolve(&coordinator);
        let daemon_config::launcher::ComposedJoin {
            launcher: combined,
            copy,
            ..
        } = active
            .prepared
            .join(
                JoinRequest {
                    option: &goal.option,
                    name: &goal.name,
                    words: &goal.selections,
                    arguments: &goal.arguments,
                },
                RunningStack {
                    selection: &active.selection,
                    launcher: &active.flat,
                },
            )
            .map_err(|e| e.to_string())?;
        let old_ids = instance_ids(&active.flat);
        let new_ids: HashSet<_> = instance_ids(&combined)
            .difference(&old_ids)
            .cloned()
            .collect();
        let placements = Placements::new(
            coordinator,
            combined
                .deployments
                .iter()
                .flat_map(|d| &d.instances)
                .map(|instance| {
                    let placement = if new_ids.contains(instance.instance_id.as_str()) {
                        host.clone()
                    } else {
                        active
                            .placements
                            .core_node_of(instance.instance_id.as_str())
                            .clone()
                    };
                    (instance.instance_id.to_string(), placement)
                })
                .collect(),
        );
        // A source the launch resolved runs the launch's node, with its
        // pins, however many joins and removals came between.
        let missing: Vec<_> = combined
            .deployments
            .iter()
            .filter(|deployment| {
                !active
                    .resolved
                    .iter()
                    .any(|item| item.deployment.source == deployment.source)
            })
            .cloned()
            .collect();
        let mut resolved = resolve_deployments(ctx, missing, &placements)
            .await
            .map_err(reason)?;
        resolve::mint_doc_pins(ctx, &mut resolved, &placements)
            .await
            .map_err(reason)?;
        let planned: Vec<_> = combined
            .deployments
            .iter()
            .map(|deployment| {
                let item = active
                    .resolved
                    .iter()
                    .chain(&resolved)
                    .find(|item| item.deployment.source == deployment.source)
                    .expect("every deployment was resolved by the launch or by this join");
                PlannedDeployment {
                    deployment: deployment.clone(),
                    ..item.clone()
                }
            })
            .collect();

        Ok(Self {
            combined,
            planned,
            placements,
            new_ids,
            host,
            copy,
            resolved,
        })
    }
}

fn instance_ids(flat: &PeppyLauncher) -> HashSet<String> {
    flat.deployments
        .iter()
        .flat_map(|d| &d.instances)
        .map(|i| i.instance_id.to_string())
        .collect()
}

/// New instances, their direct providers, and the active clock source participate in a join.
pub(super) fn join_dependencies(
    planned: &[PlannedDeployment],
    new_ids: &HashSet<String>,
    time_source: Option<&Name>,
) -> HashSet<String> {
    let providers = planned
        .iter()
        .flat_map(|item| &item.deployment.instances)
        .filter(|instance| new_ids.contains(instance.instance_id.as_str()))
        .flat_map(|instance| instance.links.values())
        .filter_map(|link| link.selection())
        .flat_map(|selection| selection.targets())
        .map(|target| {
            daemon_config::launcher::split_link_target(target)
                .0
                .to_owned()
        });
    new_ids
        .iter()
        .cloned()
        .chain(providers)
        .chain(time_source.map(Name::to_string))
        .collect()
}

/// The plan cut down to the instances `keep` accepts, deployments with none
/// left dropped.
pub(super) fn selected_instances(
    planned: &[PlannedDeployment],
    keep: impl Fn(&daemon_config::launcher::DeploymentInstance) -> bool,
) -> Vec<PlannedDeployment> {
    planned
        .iter()
        .filter_map(|item| {
            let instances: Vec<_> = item
                .deployment
                .instances
                .iter()
                .filter(|instance| keep(instance))
                .cloned()
                .collect();
            if instances.is_empty() {
                return None;
            }
            let mut selected = item.clone();
            selected.deployment.instances = instances;
            Some(selected)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::stack::fixtures::{placements_with, planned_deployment};
    use crate::services::stack::launch::clock::plan_fleet;
    use daemon_config::launcher::{LinkValue, Selection};

    /// A join takes part with every instance it brings, whatever those name
    /// as a producer, and with the stack's time source. An instance already
    /// running that the copy names nowhere stays out of it.
    #[test]
    fn a_join_depends_on_its_new_instances_their_producers_and_the_time_source() {
        let mut arm = planned_deployment("arm", &[("alpha_arm_inst", None)]);
        arm.deployment.instances[0].links.insert(
            "controller".to_owned(),
            LinkValue::Bound(Selection::Scalar("shared_inst/controller".to_owned())),
        );
        let planned = vec![
            planned_deployment("shared", &[("shared_inst", None), ("idle_inst", None)]),
            arm,
        ];
        let new_ids = HashSet::from(["alpha_arm_inst".to_owned()]);
        let sim = Name::new("sim_inst").unwrap();

        assert_eq!(
            join_dependencies(&planned, &new_ids, Some(&sim)),
            HashSet::from([
                "alpha_arm_inst".to_owned(),
                "shared_inst".to_owned(),
                "sim_inst".to_owned(),
            ])
        );
        assert_eq!(
            join_dependencies(&planned, &new_ids, None),
            HashSet::from(["alpha_arm_inst".to_owned(), "shared_inst".to_owned()]),
            "a stack on wall time has no time source to depend on"
        );
        assert!(join_dependencies(&planned, &HashSet::new(), None).is_empty());
    }

    /// The machines a change hands the time source are those of the plan it
    /// leaves running: a joined copy's machine is added, a removed copy's is
    /// dropped, and a stack left running nothing has none.
    #[test]
    fn the_fleet_a_join_and_a_removal_hand_over_is_the_plan_that_remains() {
        let placements = placements_with(
            "cn-sim",
            &[
                ("alpha_arm_inst", "cn-robot"),
                ("bravo_arm_inst", "cn-spare"),
            ],
        );
        let launched = vec![planned_deployment("engine", &[("sim_inst", None)])];
        let joined = [
            launched.clone(),
            vec![planned_deployment(
                "arm",
                &[
                    ("alpha_arm_inst", Some("cn-robot")),
                    ("bravo_arm_inst", Some("cn-spare")),
                ],
            )],
        ]
        .concat();
        let machines = |planned: &[PlannedDeployment]| {
            plan_fleet(planned, &placements)
                .expect("core node names are valid runtime names")
                .map(|fleet| fleet.iter().map(Name::to_string).collect::<Vec<_>>())
        };

        assert_eq!(machines(&launched), Some(vec!["cn-sim".to_owned()]));
        assert_eq!(
            machines(&joined),
            Some(vec![
                "cn-robot".to_owned(),
                "cn-sim".to_owned(),
                "cn-spare".to_owned(),
            ])
        );

        let after_removal = selected_instances(&joined, |instance| {
            instance.instance_id.as_str() != "bravo_arm_inst"
        });
        assert_eq!(
            machines(&after_removal),
            Some(vec!["cn-robot".to_owned(), "cn-sim".to_owned()])
        );

        let emptied = selected_instances(&joined, |_| false);
        assert!(emptied.is_empty(), "a deployment with no instance is gone");
        assert_eq!(machines(&emptied), None);
    }
}
