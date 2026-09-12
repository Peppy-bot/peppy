//! What every stack change establishes before it touches a machine: the
//! fleet the plan spans, the clock it runs on, the peers it reserves and
//! the bind sources each machine's containers need. Every refusal here
//! leaves every stack exactly as it was.

use super::PlannedDeployment;
use super::clock::{check_coordinator_clock, plan_fleet};
use super::federated::{self, ClockDemand, ReservedParticipants, SimDemandOrigin};
use crate::services::stack::action::StackChangeContext;
use crate::services::stack::container_mounts::container_mount_sources_by_machine;
use daemon_config::launcher::Placements;
use std::collections::HashMap;

/// A change cleared to touch its machines, holding their reservations.
pub(in crate::services::stack) struct ChangePlan {
    /// Every machine of the whole plan, handed to the declared time source.
    /// `None` once the plan runs nothing, which leaves no time source to hand
    /// it to.
    pub fleet: Option<config::runtime::SimTimeParticipants>,
    /// The clock the whole plan asks for; the machines that host it settle a
    /// `HostsDecide` demand ([`ReservedParticipants::established_clock`]).
    pub clock: ClockDemand,
    pub reserved: ReservedParticipants,
    /// The host paths each touched machine's containers bind.
    pub mounts: HashMap<String, Vec<String>>,
}

/// Clears a change over `plan`, of which `touched` is the part the change
/// starts, stops or depends on: the machines of `touched` are reserved,
/// under the clock `plan` demands, or the one the running stack already
/// established.
pub(in crate::services::stack) async fn preflight_change(
    ctx: &StackChangeContext,
    launch_id: &str,
    plan: &[PlannedDeployment],
    touched: &[PlannedDeployment],
    placements: &Placements,
    established: Option<&ClockDemand>,
) -> Result<ChangePlan, String> {
    let fleet = plan_fleet(plan, placements)?;
    let (requested, coordinator) = ClockDemand::of_plan(ctx, plan, fleet.as_ref());
    let clock = match established {
        Some(established) => {
            check_clock_established(established, &requested)?;
            established.clone()
        }
        None => requested,
    };
    check_coordinator_clock(coordinator, &clock, &ctx.bound_core_node)?;
    let reserved = federated::preflight(ctx, launch_id, touched, placements, &clock).await?;
    let mounts = container_mount_sources_by_machine(touched, placements)?;
    Ok(ChangePlan {
        fleet,
        clock,
        reserved,
        mounts,
    })
}

/// A stack keeps the clock its launch established: a change asking for the
/// other kind of time is refused with the fix its cause admits.
fn check_clock_established(
    established: &ClockDemand,
    requested: &ClockDemand,
) -> Result<(), String> {
    match (established, requested) {
        (ClockDemand::Wall, ClockDemand::Sim(origin)) => Err(match origin {
            SimDemandOrigin::CoordinatorClock => String::from(
                "the active stack uses wall time and this daemon serves simulated time; place \
                 the copy on a wall-time machine with --place CORE_NODE",
            ),
            SimDemandOrigin::Instance(instance) => format!(
                "the active stack uses wall time; drop `{instance}`'s `use_sim_time: true` \
                 override, or reset and launch on daemons serving simulated time"
            ),
            SimDemandOrigin::ActiveLaunch => String::from(
                "the active stack uses wall time; reset and launch on daemons serving \
                 simulated time",
            ),
        }),
        (ClockDemand::Sim(_), ClockDemand::Wall) => Err(String::from(
            "the active stack uses simulated time and this daemon serves wall time; place \
             the copy on a simulated-time machine with --place CORE_NODE",
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wall_time_stack_refuses_simulated_time_with_the_fix_its_cause_admits() {
        let wall = ClockDemand::Wall;
        assert!(check_clock_established(&wall, &ClockDemand::Wall).is_ok());
        assert!(check_clock_established(&wall, &ClockDemand::HostsDecide).is_ok());
        let coordinator =
            check_clock_established(&wall, &ClockDemand::Sim(SimDemandOrigin::CoordinatorClock))
                .unwrap_err();
        assert!(coordinator.contains("--place CORE_NODE"), "{coordinator}");
        let instance = check_clock_established(
            &wall,
            &ClockDemand::Sim(SimDemandOrigin::Instance("cam_inst".into())),
        )
        .unwrap_err();
        assert!(
            instance.contains("`cam_inst`'s `use_sim_time: true`"),
            "{instance}"
        );
        let sim = ClockDemand::Sim(SimDemandOrigin::ActiveLaunch);
        assert!(check_clock_established(&sim, &ClockDemand::HostsDecide).is_ok());
        assert!(
            check_clock_established(&sim, &ClockDemand::Sim(SimDemandOrigin::CoordinatorClock))
                .is_ok()
        );
        let wall_coordinator = check_clock_established(&sim, &ClockDemand::Wall).unwrap_err();
        assert!(
            wall_coordinator.contains("--place CORE_NODE"),
            "{wall_coordinator}"
        );
    }
}
