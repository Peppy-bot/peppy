//! `peppy stack reset`: tear the node stack down to an empty state.
//!
//! Replaces `peppy service reset`. The daemon service was already called
//! `stack_reset`; only the CLI verb was misfiled under `service`, where it read
//! as something that restarts the daemon.
//!
//! The federated mode is where "participants are discovered, not remembered"
//! pays off. Nothing here reads a coordinator's memory of who took part: it
//! asks the federation which daemons hold a slice of the launch, using the same
//! presence fan-out `stack list` already performs. That is what makes reset
//! work after a coordinator restart, and from any machine in the federation.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use core_node_api::encoding::{LaunchIdentity, StackListRequest, StackResetRequest};
use futures::future::join_all;
use tracing::info;

use crate::commands::CALLER_INSTANCE_ID;
use crate::context::AppContext;
use crate::error::{Error, Result};
use peppylib::CoreNodePresenceMessenger;
use peppylib::core_node::transport::poll;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub fn reset_stack(ctx: &Arc<AppContext>, federated: bool) -> Result<()> {
    crate::commands::block_on(reset_async(ctx, federated))
}

async fn reset_async(ctx: &Arc<AppContext>, federated: bool) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    // Read this daemon's slice identity BEFORE clearing it: it names the launch
    // to chase in federated mode, and it is what lets a participant-side reset
    // report what the rest of the system is still running.
    let slice = poll(
        &StackListRequest::new(),
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        &conn.target_core_node,
        REQUEST_TIMEOUT,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("Failed to read the stack: {e}")))?
    .launch;

    let mut targets: BTreeSet<String> = BTreeSet::from([conn.target_core_node.clone()]);

    if federated {
        // Rediscovery, not recall: ask every live core node what holds it.
        // A restarted coordinator finds its own launches again, and this works
        // from a participant just as well as from the coordinator.
        let holdings = federation_holdings(&conn).await?;
        let selected = federated_targets(&conn.target_core_node, slice.as_ref(), &holdings);
        if selected.is_empty() {
            println!(
                "Note: no live daemon reports a slice or reservation belonging to `{}`; \
                 only this daemon is reset.",
                conn.target_core_node
            );
        }
        targets.extend(selected);
        info!(
            "Resetting every held slice: {}",
            targets.iter().cloned().collect::<Vec<_>>().join(", ")
        );
    } else if let Some(launch) = &slice
        && launch.coordinator_core_node != conn.target_core_node
    {
        // Say what is NOT being cleared. A participant-side reset leaves the
        // rest of the launch running, and silence here would read as "the whole
        // thing is gone".
        println!(
            "Note: `{}` holds one slice of launch `{}`, coordinated by `{}`. This clears only \
             this daemon's slice; the other participants keep running. Use \
             `peppy stack reset --federated` to tear down the whole launch.",
            conn.target_core_node, launch.launch_id, launch.coordinator_core_node
        );
    }

    let failures: Vec<String> = join_all(targets.iter().map(|core_node| {
        let messenger = conn.messenger;
        let caller = &conn.core_node_name;
        async move {
            match poll(
                &StackResetRequest::new(),
                messenger,
                caller,
                CALLER_INSTANCE_ID,
                core_node,
                REQUEST_TIMEOUT,
            )
            .await
            {
                Ok(response) if response.success => None,
                Ok(response) => Some(format!(
                    "{core_node}: {}",
                    response
                        .error_message
                        .unwrap_or_else(|| "stack reset failed".to_owned())
                )),
                Err(e) => Some(format!("{core_node}: {e}")),
            }
        }
    }))
    .await
    .into_iter()
    .flatten()
    .collect();

    if !failures.is_empty() {
        // Report per daemon rather than collapsing to one message: a partial
        // reset leaves specific machines still populated, and the operator
        // needs to know which ones to go back to.
        return Err(Error::ExecutionFailed(format!(
            "stack reset failed on {} of {} daemon(s):\n  {}",
            failures.len(),
            targets.len(),
            failures.join("\n  ")
        )));
    }

    info!(
        "Reset {} daemon(s): {}",
        targets.len(),
        targets.iter().cloned().collect::<Vec<_>>().join(", ")
    );
    Ok(())
}

/// What one live daemon reports holding: the launch its slice came from, and
/// the launch currently reserving it.
struct DaemonHoldings {
    core_node: String,
    slice: Option<LaunchIdentity>,
    reservation: Option<LaunchIdentity>,
}

/// Every live core node's self-reported holdings.
///
/// A daemon that cannot be reached is deliberately NOT dropped silently: it
/// simply does not appear here, and if it was a participant its slice stays up.
/// That is reported by the reset attempt on the daemons that did answer rather
/// than being papered over as a successful full teardown.
async fn federation_holdings(
    conn: &crate::context::DaemonConnection<'_>,
) -> Result<Vec<DaemonHoldings>> {
    let live = CoreNodePresenceMessenger::list_live(
        conn.messenger,
        None,
        CoreNodePresenceMessenger::LIST_TIMEOUT,
    )
    .await?;

    let core_nodes: BTreeSet<String> = live.into_iter().map(|claim| claim.core_node).collect();

    Ok(join_all(core_nodes.into_iter().map(|core_node| {
        let messenger = conn.messenger;
        let caller = &conn.core_node_name;
        async move {
            let response = poll(
                &StackListRequest::new(),
                messenger,
                caller,
                CALLER_INSTANCE_ID,
                &core_node,
                REQUEST_TIMEOUT,
            )
            .await
            .ok()?;
            Some(DaemonHoldings {
                core_node,
                slice: response.launch,
                reservation: response.reservation,
            })
        }
    }))
    .await
    .into_iter()
    .flatten()
    .collect())
}

/// Which daemons a federated reset must clear, given what every live daemon
/// self-reports.
///
/// Keyed on the launch the target's slice names when it has one, and on the
/// target's own NAME otherwise: a coordinator restart wipes its in-memory
/// slice, but every machine its launches touched still names it as
/// coordinator, so rediscovery keys on the one fact that survives. A target
/// that is itself the coordinator sweeps by both keys, catching machines a
/// stale earlier launch of its own still holds.
///
/// Reservations count the same as slices: a machine whose launch died before
/// populating a slice holds only a reservation, and it is exactly the machine
/// every new launch bounces off.
fn federated_targets(
    target_core_node: &str,
    target_slice: Option<&LaunchIdentity>,
    holdings: &[DaemonHoldings],
) -> BTreeSet<String> {
    let held_by_launch = |daemon: &DaemonHoldings, launch_id: &str| {
        let names_it = |identity: &Option<LaunchIdentity>| {
            identity
                .as_ref()
                .is_some_and(|identity| identity.launch_id == launch_id)
        };
        names_it(&daemon.slice) || names_it(&daemon.reservation)
    };
    let held_for_coordinator = |daemon: &DaemonHoldings| {
        let names_it = |identity: &Option<LaunchIdentity>| {
            identity
                .as_ref()
                .is_some_and(|identity| identity.coordinator_core_node == target_core_node)
        };
        names_it(&daemon.slice) || names_it(&daemon.reservation)
    };

    holdings
        .iter()
        .filter(|daemon| match target_slice {
            Some(slice) if slice.coordinator_core_node == target_core_node => {
                held_by_launch(daemon, &slice.launch_id) || held_for_coordinator(daemon)
            }
            Some(slice) => held_by_launch(daemon, &slice.launch_id),
            None => held_for_coordinator(daemon),
        })
        .map(|daemon| daemon.core_node.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn daemon(
        core_node: &str,
        slice: Option<(&str, &str)>,
        reservation: Option<(&str, &str)>,
    ) -> DaemonHoldings {
        let identity =
            |pair: Option<(&str, &str)>| pair.map(|(id, coord)| LaunchIdentity::new(id, coord));
        DaemonHoldings {
            core_node: core_node.to_owned(),
            slice: identity(slice),
            reservation: identity(reservation),
        }
    }

    /// From a participant, the reset chases exactly the launch its slice
    /// names: machines holding a slice OR a reservation of that launch are
    /// swept, machines belonging to other launches are not.
    #[test]
    fn a_participants_reset_chases_its_launch_across_slices_and_reservations() {
        let holdings = [
            daemon("cn-participant", Some(("launch-a", "cn-coord")), None),
            daemon("cn-sliced", Some(("launch-a", "cn-coord")), None),
            daemon("cn-reserved-only", None, Some(("launch-a", "cn-coord"))),
            daemon("cn-other-launch", Some(("launch-b", "cn-elsewhere")), None),
        ];
        let targets = federated_targets(
            "cn-participant",
            Some(&LaunchIdentity::new("launch-a", "cn-coord")),
            &holdings,
        );
        assert_eq!(
            targets,
            BTreeSet::from([
                "cn-participant".to_owned(),
                "cn-sliced".to_owned(),
                "cn-reserved-only".to_owned(),
            ])
        );
    }

    /// From the coordinator, the reset also sweeps by its own name: a machine
    /// still held by an EARLIER launch of this coordinator (its release was
    /// lost) names a launch id nobody's slice can produce anymore, so the
    /// coordinator's name is the only key that finds it.
    #[test]
    fn a_coordinators_reset_also_sweeps_stale_holds_of_its_earlier_launches() {
        let holdings = [
            daemon("cn-coord", Some(("launch-b", "cn-coord")), None),
            daemon("cn-current", Some(("launch-b", "cn-coord")), None),
            daemon("cn-wedged", None, Some(("launch-a", "cn-coord"))),
            daemon("cn-foreign", Some(("launch-x", "cn-elsewhere")), None),
        ];
        let targets = federated_targets(
            "cn-coord",
            Some(&LaunchIdentity::new("launch-b", "cn-coord")),
            &holdings,
        );
        assert_eq!(
            targets,
            BTreeSet::from([
                "cn-coord".to_owned(),
                "cn-current".to_owned(),
                "cn-wedged".to_owned(),
            ])
        );
    }

    /// A restarted or already-reset coordinator has no slice left to name a
    /// launch, so rediscovery keys on its NAME: every machine whose slice or
    /// reservation names it as coordinator is swept. This is the state the
    /// old slice-keyed discovery could not recover from.
    #[test]
    fn a_coordinator_with_no_slice_still_finds_every_machine_its_launches_hold() {
        let holdings = [
            daemon("cn-sliced", Some(("launch-a", "cn-coord")), None),
            daemon("cn-wedged", None, Some(("launch-old", "cn-coord"))),
            daemon("cn-foreign", None, Some(("launch-x", "cn-elsewhere"))),
        ];
        let targets = federated_targets("cn-coord", None, &holdings);
        assert_eq!(
            targets,
            BTreeSet::from(["cn-sliced".to_owned(), "cn-wedged".to_owned()])
        );
    }

    /// A participant's launch-keyed reset must not sweep by coordinator name:
    /// that coordinator may be driving a CURRENT healthy launch on other
    /// machines, which this reset has no standing to tear down.
    #[test]
    fn a_participants_reset_leaves_the_coordinators_other_launches_alone() {
        let holdings = [
            daemon("cn-participant", Some(("launch-old", "cn-coord")), None),
            daemon("cn-current", Some(("launch-new", "cn-coord")), None),
        ];
        let targets = federated_targets(
            "cn-participant",
            Some(&LaunchIdentity::new("launch-old", "cn-coord")),
            &holdings,
        );
        assert_eq!(targets, BTreeSet::from(["cn-participant".to_owned()]));
    }

    #[test]
    fn nothing_held_selects_nothing() {
        let holdings = [daemon("cn-idle", None, None)];
        assert!(federated_targets("cn-coord", None, &holdings).is_empty());
    }
}
