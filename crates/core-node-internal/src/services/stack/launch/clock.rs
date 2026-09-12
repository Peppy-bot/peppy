//! The clock a stack change runs on: the machines its instances span, whether
//! the coordinator may host it, and the instance publishing simulated time.

use super::feedback::{publish_stderr, publish_stdout};
use super::{NodeKey, PlannedDeployment, federated};
use crate::services::stack::action::StackChangeContext;
use crate::services::stack::{ChangeResult, STACK_QUERY_TIMEOUT};
use config::runtime::{CoreNodeName, Name};
use core_node_api::encoding::LaunchFeedbackStep;
use daemon_config::launcher::Placements;
use peppylib::messaging::SenderTarget;

/// The machines of the plan: every core node at least one planned instance
/// runs on, parsed into the participant set a time source is handed. `None`
/// when the plan runs no instance, the stack a launcher left holding no copy
/// keeps, which has no time source to hand anything to.
///
/// A core-node name and a runtime `Name` share one character set, so the
/// conversion cannot fail for a name that reached a `Placements`; the error
/// path exists so that invariant is checked, not assumed, and it is checked
/// before anything is torn down.
pub(in crate::services::stack) fn plan_fleet(
    planned: &[PlannedDeployment],
    placements: &daemon_config::launcher::Placements,
) -> std::result::Result<Option<config::runtime::SimTimeParticipants>, String> {
    let instance_ids: Vec<&str> = planned
        .iter()
        .flat_map(|item| &item.deployment.instances)
        .map(|instance| instance.instance_id.as_str())
        .collect();
    if instance_ids.is_empty() {
        return Ok(None);
    }
    let names = placements
        .participants(instance_ids)
        .into_iter()
        .map(|core_node| {
            config::runtime::Name::new(core_node)
                .map_err(|e| format!("core node `{core_node}` is not a valid runtime name: {e}"))
        })
        .collect::<std::result::Result<Vec<_>, String>>()?;
    config::runtime::SimTimeParticipants::try_from(names)
        .map(Some)
        .map_err(|e| format!("the change resolved no valid machine set: {e}"))
}

/// The coordinator's own half of the clock-agreement rule the federated
/// preflight applies to each peer, asked only when the coordinator is one of
/// the fleet's machines. A machine running none of the launch is not its
/// business.
///
/// Only one refusal is reachable here: a wall-serving coordinator hosting a
/// simulated launch, whose own ticks would feed the key those instances
/// read. The demand is read off this same daemon, so a wall demand implies
/// this daemon serves wall time; the mirror refusal (a sim machine in a wall
/// launch) exists for peers alone, in the preflight.
pub(in crate::services::stack) fn check_coordinator_clock(
    coordinator: federated::Coordinator,
    clock_demand: &federated::ClockDemand,
    name: &str,
) -> std::result::Result<(), String> {
    if !coordinator.hosts {
        return Ok(());
    }
    federated::check_clock_agreement(name, coordinator.serves_sim_time, clock_demand)
}

/// One advisory line for each clock state a launch accepts but an operator
/// probably did not intend; the states a launch refuses already speak for
/// themselves.
///
/// A declared source in a wall launch resolves inert
/// ([`config::runtime::NodeInstancePlan::resolve`]) so the launch succeeds
/// silently; say so, or a forgotten `--clock-source=sim` is discoverable only
/// from diverging timestamps. A simulated launch declaring no source starts,
/// and then every sim-time instance waits at `clock not ready` for a tick
/// nothing in the launch publishes; say who is missing. A fully-placed launch
/// (`ClockDemand::HostsDecide`) learns its clock from its machines at
/// preflight, after this point, so it gets no advisory here.
pub(super) async fn advise_on_clock_shape(
    ctx: &StackChangeContext,
    planned: &[PlannedDeployment],
    clock_demand: &federated::ClockDemand,
) {
    let declared_source = planned
        .iter()
        .flat_map(|item| &item.deployment.instances)
        .find(|instance| instance.framework.publishes_sim_time);
    match (declared_source, clock_demand) {
        // Name the source on every launch that may run on its clock: a
        // declared source that never publishes then hangs the fleet at
        // `clock not ready` with a named suspect in the launch output.
        (Some(instance), federated::ClockDemand::Sim(_) | federated::ClockDemand::HostsDecide) => {
            publish_stdout(
                ctx,
                format!(
                    "instance `{}` is this launch's source of simulated time",
                    instance.instance_id
                ),
                LaunchFeedbackStep::LauncherStep,
            )
            .await;
        }
        (Some(instance), federated::ClockDemand::Wall) => {
            publish_stdout(
                ctx,
                format!(
                    "instance `{}` declares a simulated-time source, but this launch runs on \
                     wall time, so it will publish nothing. Start the coordinating daemon with \
                     `peppy service serve --clock-source=sim` to run on its clock.",
                    instance.instance_id
                ),
                LaunchFeedbackStep::LauncherStep,
            )
            .await;
        }
        (None, federated::ClockDemand::Sim(_)) => {
            publish_stderr(
                ctx,
                NO_TIME_SOURCE.to_owned(),
                LaunchFeedbackStep::LauncherStep,
            )
            .await;
        }
        _ => {}
    }
}

/// The warning for a simulated-time stack with no declared source.
const NO_TIME_SOURCE: &str = "this simulated launch declares no time source (no instance sets \
                              `framework: { publishes_sim_time: true }`): every sim-time instance \
                              will wait at `clock not ready` until an external publisher feeds \
                              each machine's `clock` topic.";

/// A join onto a simulated-time stack that declares no source warns, as the
/// launch did: the copy's sim-time instances wait at `clock not ready`.
pub(in crate::services::stack) async fn warn_when_no_time_source(
    ctx: &StackChangeContext,
    planned: &[PlannedDeployment],
    clock_demand: &federated::ClockDemand,
) {
    let declared = planned
        .iter()
        .flat_map(|item| &item.deployment.instances)
        .any(|instance| instance.framework.publishes_sim_time);
    if !declared && matches!(clock_demand, federated::ClockDemand::Sim(_)) {
        publish_stderr(
            ctx,
            NO_TIME_SOURCE.to_owned(),
            LaunchFeedbackStep::LauncherStep,
        )
        .await;
    }
}

#[derive(Debug, Clone)]
pub(in crate::services::stack) struct TimeSource {
    pub(in crate::services::stack) core_node: CoreNodeName,
    pub(in crate::services::stack) instance_id: Name,
    node: NodeKey,
}

impl TimeSource {
    pub(in crate::services::stack) fn of(
        ctx: &StackChangeContext,
        planned: &[PlannedDeployment],
        placements: &Placements,
        peers: &federated::ReservedParticipants,
    ) -> Option<Self> {
        planned.iter().find_map(|item| {
            item.deployment.instances.iter().find_map(|instance| {
                let host = placements.of(instance.instance_id.as_str());
                let use_sim_time = instance.framework.use_sim_time.unwrap_or_else(|| {
                    peers
                        .serves_sim_time(host)
                        .unwrap_or(ctx.daemon_defaults.use_sim_time)
                });
                (instance.framework.publishes_sim_time && use_sim_time).then(|| Self {
                    core_node: placements
                        .core_node_of(instance.instance_id.as_str())
                        .clone(),
                    instance_id: instance.instance_id.clone(),
                    node: NodeKey::new(&item.node_name, &item.node_tag),
                })
            })
        })
    }

    pub(in crate::services::stack) async fn set_participants(
        &self,
        ctx: &StackChangeContext,
        participants: config::runtime::SimTimeParticipants,
    ) -> ChangeResult<()> {
        use core_node_api::encoding::{SimTimeParticipantsRequest, SimTimeParticipantsResponse};
        let address =
            config::runtime::ProducerRef::new(self.core_node.as_str(), self.instance_id.as_str());
        let response = peppylib::ServiceMessenger::poll(
            &ctx.messenger,
            &ctx.bound_core_node,
            &ctx.core_instance_id,
            SenderTarget::node(&self.node.name, &self.node.tag).map_err(|e| e.to_string())?,
            core_node_api::ServiceId::SimTimeParticipants.name(),
            peppylib::messaging::ServiceTarget::Producer(&address),
            SimTimeParticipantsRequest { participants }
                .encode()
                .map_err(|e| e.to_string())?,
            STACK_QUERY_TIMEOUT,
        )
        .await
        .map_err(|e| {
            format!(
                "cannot update simulation time source `{}` on `{}`: {e}; check that its node was rebuilt with this peppy version",
                self.instance_id, self.core_node
            )
        })?;
        SimTimeParticipantsResponse::decode(response.payload_bytes().as_ref())
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::stack::fixtures::{placements_with, planned_deployment};

    /// The fan-out list is every machine the launch actually runs on, the
    /// coordinator included when it hosts something, each named once however
    /// many instances it holds. This is what makes the clock reach a fleet
    /// without any launcher naming a machine, and what keeps it right when a
    /// launch grows more instances.
    #[test]
    fn the_sim_time_participants_are_every_machine_of_the_launch() {
        let planned = vec![
            planned_deployment("engine", &[("sim_inst", None)]),
            planned_deployment(
                "arm",
                &[
                    ("arm_a", Some("cn-robot-a")),
                    ("arm_b", Some("cn-robot-b")),
                    ("arm_c", Some("cn-robot-b")),
                ],
            ),
        ];
        let placements = placements_with(
            "cn-sim",
            &[
                ("arm_a", "cn-robot-a"),
                ("arm_b", "cn-robot-b"),
                ("arm_c", "cn-robot-b"),
            ],
        );

        let fleet = plan_fleet(&planned, &placements)
            .expect("core node names are valid runtime names")
            .expect("the plan runs instances");
        assert_eq!(
            fleet.iter().map(|name| name.as_str()).collect::<Vec<_>>(),
            ["cn-robot-a", "cn-robot-b", "cn-sim"]
        );

        // A single-machine launch is the same derivation with one answer.
        let solo = vec![planned_deployment("engine", &[("sim_inst", None)])];
        let solo_fleet = plan_fleet(&solo, &placements_with("cn-solo", &[]))
            .expect("valid names")
            .expect("the plan runs instances");
        assert_eq!(
            solo_fleet
                .iter()
                .map(|name| name.as_str())
                .collect::<Vec<_>>(),
            ["cn-solo"]
        );

        // A plan left running nothing spans no machine, so there is no
        // participant set to build.
        assert_eq!(plan_fleet(&[], &placements), Ok(None));
    }

    /// The one refusal reachable at the coordinator (a wall daemon hosting a
    /// simulated launch) fires, and only when the coordinator is one of the
    /// launch's machines.
    #[test]
    fn a_coordinator_hosting_part_of_a_launch_must_agree_with_its_clock() {
        let sim_demand =
            federated::ClockDemand::Sim(federated::SimDemandOrigin::Instance("relay".to_owned()));
        let wall_host = federated::Coordinator {
            serves_sim_time: false,
            hosts: true,
        };

        let error = check_coordinator_clock(wall_host, &sim_demand, "cn-sim")
            .expect_err("a wall coordinator cannot host a simulated launch");
        assert!(error.contains("`cn-sim`"), "got: {error}");
        assert!(error.contains("--clock-source=sim"), "got: {error}");
        assert!(
            error.contains("instance `relay`"),
            "the refusal names what committed the launch: {error}"
        );

        assert!(
            check_coordinator_clock(wall_host, &federated::ClockDemand::Wall, "cn-sim").is_ok(),
            "a wall coordinator hosts a wall launch"
        );
        assert!(
            check_coordinator_clock(
                federated::Coordinator {
                    serves_sim_time: true,
                    hosts: true,
                },
                &federated::ClockDemand::Sim(federated::SimDemandOrigin::CoordinatorClock),
                "cn-sim",
            )
            .is_ok(),
            "a sim-mode coordinator hosts a simulated launch"
        );

        // Everything placed on peers: the coordinator is not of the fleet, so
        // its own clock mode is not this launch's business, in either mode.
        for serves_sim_time in [false, true] {
            for demand in [
                sim_demand.clone(),
                federated::ClockDemand::Wall,
                federated::ClockDemand::HostsDecide,
            ] {
                assert!(
                    check_coordinator_clock(
                        federated::Coordinator {
                            serves_sim_time,
                            hosts: false,
                        },
                        &demand,
                        "cn-sim",
                    )
                    .is_ok(),
                    "a coordinator hosting nothing is never refused"
                );
            }
        }
    }

    fn plan_with(use_sim_time: Option<bool>) -> config::runtime::NodeInstancePlan {
        config::runtime::NodeInstancePlan {
            use_sim_time,
            ..config::runtime::NodeInstancePlan::new(
                config::runtime::Name::new("inst_1").expect("valid name"),
            )
        }
    }

    /// A per-instance override resolves against the daemon default on the
    /// daemon that spawns the node, because only it knows its own default. A
    /// plan shipped from another machine carries the override unresolved,
    /// which is why this is tested on the plan rather than on a launcher-side
    /// helper. `Some(false)` forces wall time on a sim-mode daemon; the other
    /// direction is not an override but a refusal, since a wall-mode daemon
    /// cannot serve the simulated time the instance asked for.

    #[test]
    fn a_per_instance_use_sim_time_override_resolves_on_the_spawning_daemon() {
        assert!(
            !plan_with(Some(false))
                .resolve(true)
                .expect("forcing wall time on a sim daemon is legal")
                .framework
                .use_sim_time
        );
        assert!(
            plan_with(Some(true))
                .resolve(true)
                .expect("sim time on a sim daemon resolves")
                .framework
                .use_sim_time
        );
        let refused = plan_with(Some(true))
            .resolve(false)
            .expect_err("sim time on a wall daemon is refused, not resolved");
        assert_eq!(refused.instance_id, "inst_1");
    }

    /// When the instance omits the override, the spawning daemon decides.
    #[test]
    fn an_absent_override_falls_through_to_the_daemon_default() {
        assert!(
            !plan_with(None)
                .resolve(false)
                .expect("wall on wall resolves")
                .framework
                .use_sim_time
        );
        assert!(
            plan_with(None)
                .resolve(true)
                .expect("sim on sim resolves")
                .framework
                .use_sim_time
        );
    }
}
