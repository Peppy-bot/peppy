//! Coordinator side of a federated launch: preflight, partitioning, and the
//! wave dispatch that keeps consumers starting after their producers across
//! machine boundaries.
//!
//! # Reserve, do not poll
//!
//! Nothing is torn down until every participant is RESERVED. A preflight that
//! merely asks whether a participant is busy is a time-of-check / time-of-use
//! race: two coordinators can both observe idle, both begin dispatching, and
//! the per-daemon gate only refuses the second one partway through, after
//! machines have already had their stacks replaced. The failure mode is several
//! machines left in an unknown state by a launch that never had a chance of
//! succeeding.
//!
//! Reserving first is the same reserve-then-commit discipline the pairing path
//! uses, applied one level up where the blast radius is a whole machine rather
//! than one slot. The reservation is a lease held against this coordinator's
//! presence, so a coordinator that dies mid-launch frees the machines it held
//! rather than wedging them (see `services::federation`).
//!
//! # The coordinator is the serialization point
//!
//! Anything that must be atomic across daemons is sequenced by this module's
//! own single-threaded plan. Daemons never run distributed transactions among
//! themselves; daemon-to-daemon runtime traffic is best-effort idempotent
//! notification only.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use core_node_api::encoding::{
    ParticipantReleaseRequest, ParticipantReserveRequest, ParticipantReserveResponse,
    ResolvedManifest,
};
use daemon_config::launcher::{Deployment, DeploymentSource, Placements};
use futures::future::join_all;
use peppylib::core_node::transport::poll;
use peppylib::{CoreNodePresenceMessenger, MessengerHandle};

/// Bound on one daemon-to-daemon preflight call. The peer answers from its own
/// caches, so a healthy one answers well inside this; the budget exists so an
/// unreachable peer fails the launch instead of hanging it.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on a release. Shorter than a reserve because it does no resolution
/// work, and it runs on the unwind path where waiting helps nobody.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(10);

/// What a coordinator learned from one participant during preflight.
#[derive(Debug, Clone)]
pub(crate) struct ParticipantSlice {
    pub(crate) core_node: String,
    pub(crate) peppy_version: String,
    pub(crate) root_instance_id: String,
    /// One per deployment placed on this participant, in the order they were
    /// requested, so the coordinator can align them with its own plan.
    pub(crate) manifests: Vec<ResolvedManifest>,
    /// The deployments this participant hosts, in manifest order.
    pub(crate) deployments: Vec<Deployment>,
}

/// Everything preflight established, ready for validation and dispatch.
#[derive(Debug, Default)]
pub(crate) struct Preflight {
    pub(crate) slices: Vec<ParticipantSlice>,
}

/// Which deployments each participant hosts.
///
/// A deployment goes to every core node hosting at least one of its instances,
/// because that daemon must add and build the node before starting its share.
/// One node split across daemons is therefore added on both, which is exactly
/// what "several placed instances under one deployment" means operationally.
pub(crate) fn partition_deployments(
    deployments: &[Deployment],
    placements: &Placements,
    coordinator: &str,
) -> BTreeMap<String, Vec<Deployment>> {
    let mut by_core_node: BTreeMap<String, Vec<Deployment>> = BTreeMap::new();
    for deployment in deployments {
        let mut hosts: BTreeMap<String, Vec<_>> = BTreeMap::new();
        for instance in &deployment.instances {
            hosts
                .entry(placements.of(instance.instance_id.as_str()).to_owned())
                .or_default()
                .push(instance.clone());
        }
        // A deployment with no instances at all still belongs somewhere; put it
        // on the coordinator so it is added rather than silently dropped.
        if hosts.is_empty() {
            hosts.insert(coordinator.to_owned(), Vec::new());
        }
        for (core_node, instances) in hosts {
            by_core_node
                .entry(core_node)
                .or_default()
                .push(Deployment {
                    source: deployment.source.clone(),
                    instances,
                });
        }
    }
    by_core_node
}

/// Refuses any wired core node that is not live on the federation.
///
/// Liveness is read from zenoh presence, not from the platform HTTP roster: a
/// launch depends on being able to TALK to a machine right now, which the
/// roster does not attest. `peppy platform list` stays the human-facing view.
pub(crate) async fn reject_unreachable_core_nodes(
    messenger: &MessengerHandle,
    wanted: &BTreeSet<String>,
) -> std::result::Result<(), String> {
    let live: BTreeSet<String> = CoreNodePresenceMessenger::list_live(
        messenger,
        None,
        CoreNodePresenceMessenger::LIST_TIMEOUT,
    )
    .await
    .map_err(|e| format!("could not enumerate the federation: {e}"))?
    .into_iter()
    .map(|claim| claim.core_node)
    .collect();

    let missing: Vec<&String> = wanted.difference(&live).collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "these core nodes are not live on the federation: {}. Live right now: {}. \
         Check `peppy platform list`, and that each machine's daemon is running and \
         logged into this workspace.",
        missing
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", "),
        if live.is_empty() {
            "nothing".to_owned()
        } else {
            live.iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        }
    ))
}

/// Reserves every peer participant and collects what only they can report.
///
/// All-or-nothing: on any refusal the reservations already obtained are
/// released before the error returns, so a failed preflight leaves no machine
/// held. Only after a full set of acks does anything get torn down anywhere.
pub(crate) async fn reserve_participants(
    messenger: &MessengerHandle,
    coordinator: &str,
    caller_instance_id: &str,
    launch_id: &str,
    slices: &BTreeMap<String, Vec<Deployment>>,
    own_version: &str,
) -> std::result::Result<Preflight, String> {
    let peers: Vec<(&String, &Vec<Deployment>)> = slices
        .iter()
        .filter(|(core_node, _)| core_node.as_str() != coordinator)
        .collect();

    let acks = join_all(peers.iter().map(|(core_node, deployments)| {
        let request = build_reserve_request(launch_id, coordinator, deployments);
        async move {
            let outcome = match request {
                Ok(request) => poll(
                    &request,
                    messenger,
                    coordinator,
                    caller_instance_id,
                    core_node,
                    PREFLIGHT_TIMEOUT,
                )
                .await
                .map_err(|e| format!("`{core_node}` did not answer the reservation: {e}")),
                Err(reason) => Err(format!("`{core_node}`: {reason}")),
            };
            ((*core_node).clone(), (*deployments).clone(), outcome)
        }
    }))
    .await;

    let mut preflight = Preflight::default();
    let mut reserved: Vec<String> = Vec::new();
    let mut refusals: Vec<String> = Vec::new();

    for (core_node, deployments, outcome) in acks {
        match outcome {
            Ok(response) if response.accepted => {
                reserved.push(core_node.clone());
                match check_version(&core_node, &response, own_version) {
                    Ok(()) => preflight.slices.push(ParticipantSlice {
                        core_node,
                        peppy_version: response.peppy_version,
                        root_instance_id: response.root_instance_id,
                        manifests: response.manifests,
                        deployments,
                    }),
                    Err(reason) => refusals.push(reason),
                }
            }
            Ok(response) => refusals.push(format!(
                "`{core_node}` refused: {}",
                response
                    .rejection_reason
                    .unwrap_or_else(|| "no reason given".to_owned())
            )),
            Err(reason) => refusals.push(reason),
        }
    }

    if refusals.is_empty() {
        return Ok(preflight);
    }

    // Release everything obtained before failing. This is the whole point of
    // reserving first: a launch that cannot proceed must leave every machine
    // exactly as it found it.
    release_participants(
        messenger,
        coordinator,
        caller_instance_id,
        launch_id,
        &reserved,
    )
    .await;

    Err(format!(
        "federated launch preflight failed; no stack was touched:\n  {}",
        refusals.join("\n  ")
    ))
}

/// A mixed-version federation is refused before any stack is touched.
///
/// The wire between two peppy versions is a hard break with no compatibility
/// path, so the only safe outcome is to name the mismatch and stop.
fn check_version(
    core_node: &str,
    response: &ParticipantReserveResponse,
    own_version: &str,
) -> std::result::Result<(), String> {
    if response.peppy_version == own_version {
        return Ok(());
    }
    Err(format!(
        "`{core_node}` runs peppy {} but this coordinator runs {own_version}. A federated \
         launch requires the same version on every participant; upgrade the older machine.",
        response.peppy_version
    ))
}

fn build_reserve_request(
    launch_id: &str,
    coordinator: &str,
    deployments: &[Deployment],
) -> std::result::Result<ParticipantReserveRequest, String> {
    let mut sources = Vec::with_capacity(deployments.len());
    for deployment in deployments {
        if let DeploymentSource::Local(spec) = &deployment.source {
            return Err(format!(
                "`local:{}` cannot be placed on another core node: the path names a tree on \
                 the coordinator's filesystem. A `local:` deployment must keep all of its \
                 instances on one core node; publish the node to a repo or url source to \
                 split it across machines.",
                spec.local.display()
            ));
        }
        sources.push(
            serde_json5::to_string(&deployment.source)
                .map_err(|e| format!("could not encode a deployment source: {e}"))?,
        );
    }
    Ok(ParticipantReserveRequest::new(launch_id, coordinator)
        .with_deployment_sources(sources))
}

/// Best-effort release of every named participant. Failures are logged, not
/// returned: this runs on paths that are already failing or already finished,
/// and a release that does not land is covered by the presence lease.
pub(crate) async fn release_participants(
    messenger: &MessengerHandle,
    coordinator: &str,
    caller_instance_id: &str,
    launch_id: &str,
    participants: &[String],
) {
    join_all(participants.iter().map(|core_node| async move {
        let result = poll(
            &ParticipantReleaseRequest::new(launch_id),
            messenger,
            coordinator,
            caller_instance_id,
            core_node,
            RELEASE_TIMEOUT,
        )
        .await;
        if let Err(e) = result {
            tracing::warn!(
                "could not release `{core_node}` from launch `{launch_id}` ({e}); its \
                 reservation drops on its own when this coordinator leaves the federation"
            );
        }
    }))
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use daemon_config::launcher::DeploymentInstance;

    fn instance(id: &str) -> DeploymentInstance {
        DeploymentInstance::empty(config::runtime::Name::new(id).expect("valid name"))
    }

    fn deployment(name: &str, instances: Vec<DeploymentInstance>) -> Deployment {
        Deployment {
            source: serde_json5::from_str(&format!(r#"{{ name: "{name}", tag: "v1" }}"#))
                .expect("valid repo source"),
            instances,
        }
    }

    fn placements(pairs: &[(&str, &str)]) -> Placements {
        Placements::new(
            "cn-robot",
            pairs
                .iter()
                .map(|(id, core_node)| ((*id).to_owned(), (*core_node).to_owned()))
                .collect(),
        )
    }

    #[test]
    fn a_single_machine_launch_partitions_onto_the_coordinator() {
        let deployments = vec![deployment("uvc_camera", vec![instance("cam_1")])];
        let partitioned = partition_deployments(
            &deployments,
            &Placements::all_on("cn-robot"),
            "cn-robot",
        );
        assert_eq!(partitioned.len(), 1);
        assert_eq!(partitioned["cn-robot"].len(), 1);
    }

    #[test]
    fn instances_are_partitioned_by_their_placement() {
        let deployments = vec![
            deployment("uvc_camera", vec![instance("cam_1")]),
            deployment("planner", vec![instance("planner_1")]),
        ];
        let partitioned = partition_deployments(
            &deployments,
            &placements(&[("planner_1", "cn-atlas")]),
            "cn-robot",
        );
        assert_eq!(
            partitioned.keys().cloned().collect::<Vec<_>>(),
            ["cn-atlas", "cn-robot"]
        );
        assert_eq!(partitioned["cn-robot"][0].instances[0].instance_id.as_str(), "cam_1");
        assert_eq!(
            partitioned["cn-atlas"][0].instances[0].instance_id.as_str(),
            "planner_1"
        );
    }

    /// One node whose instances straddle two participants is added on both,
    /// each with only its own share of the instances. That is what makes "a
    /// node split across daemons is several placed instances under one
    /// deployment" work: both daemons need the node present to start theirs.
    #[test]
    fn a_deployment_straddling_two_daemons_lands_on_both_with_its_own_instances() {
        let deployments = vec![deployment(
            "uvc_camera",
            vec![instance("cam_robot"), instance("cam_cloud")],
        )];
        let partitioned = partition_deployments(
            &deployments,
            &placements(&[("cam_cloud", "cn-atlas")]),
            "cn-robot",
        );

        assert_eq!(partitioned["cn-robot"].len(), 1);
        assert_eq!(partitioned["cn-robot"][0].instances.len(), 1);
        assert_eq!(
            partitioned["cn-robot"][0].instances[0].instance_id.as_str(),
            "cam_robot"
        );

        assert_eq!(partitioned["cn-atlas"].len(), 1);
        assert_eq!(partitioned["cn-atlas"][0].instances.len(), 1);
        assert_eq!(
            partitioned["cn-atlas"][0].instances[0].instance_id.as_str(),
            "cam_cloud"
        );
    }

    #[test]
    fn a_local_source_placed_off_coordinator_is_refused_before_dispatch() {
        let local = Deployment {
            source: serde_json5::from_str(r#"{ local: "./nodes/planner" }"#)
                .expect("valid local source"),
            instances: vec![instance("planner_1")],
        };
        let error = build_reserve_request("launch-a", "cn-robot", &[local])
            .expect_err("a local source cannot cross machines");
        assert!(error.contains("names a tree on"), "got: {error}");
        assert!(error.contains("repo or url source"), "got: {error}");
    }

    #[test]
    fn a_host_independent_source_is_sent_verbatim_for_the_peer_to_resolve() {
        let request = build_reserve_request(
            "launch-a",
            "cn-robot",
            &[deployment("deliberative_planner", vec![instance("planner_1")])],
        )
        .expect("a repo source crosses machines");
        assert_eq!(request.launch_id, "launch-a");
        assert_eq!(request.coordinator_core_node, "cn-robot");
        assert_eq!(request.deployment_sources_json5.len(), 1);
        assert!(
            request.deployment_sources_json5[0].contains("deliberative_planner"),
            "got: {:?}",
            request.deployment_sources_json5
        );
    }

    #[test]
    fn a_version_mismatch_is_refused_and_names_both_versions() {
        let response = ParticipantReserveResponse::accepted("v0.19.0", "gen_1", Vec::new());
        let error =
            check_version("cn-atlas", &response, "v0.20.0").expect_err("skew must be refused");
        assert!(error.contains("v0.19.0"), "got: {error}");
        assert!(error.contains("v0.20.0"), "got: {error}");
        assert!(error.contains("same version"), "got: {error}");
    }

    #[test]
    fn a_matching_version_passes() {
        let response = ParticipantReserveResponse::accepted("v0.20.0", "gen_1", Vec::new());
        assert!(check_version("cn-atlas", &response, "v0.20.0").is_ok());
    }
}

/// What a federated launch established before anything was torn down.
pub(super) struct FederatedLaunch {
    /// Empty for a single-machine launch.
    pub(super) participants: Vec<ParticipantSlice>,
}

/// The whole preflight, in the order the plan requires.
///
/// Runs BEFORE the teardown that a launch performs, so every refusal here
/// leaves the coordinator's existing stack, and every participant's, exactly as
/// it was. That ordering is the feature: a launch that cannot succeed must not
/// cost you the stack you already had.
pub(super) async fn preflight(
    ctx: &super::ProcessLaunchContext,
    goal: &core_node_api::encoding::LaunchGoal,
    planned: &[super::PlannedDeployment],
    placements: &Placements,
) -> std::result::Result<FederatedLaunch, String> {
    if !placements.is_federated() {
        return Ok(FederatedLaunch {
            participants: Vec::new(),
        });
    }

    let deployments: Vec<Deployment> = planned
        .iter()
        .map(|item| item.deployment.clone())
        .collect();
    let slices = partition_deployments(&deployments, placements, ctx.bound_core_node.as_str());

    let peers: BTreeSet<String> = slices
        .keys()
        .filter(|core_node| core_node.as_str() != ctx.bound_core_node.as_str())
        .cloned()
        .collect();

    // A wired core node is validated by live zenoh presence, not the platform
    // HTTP roster: what a launch needs is to be able to talk to the machine
    // right now, which the roster does not attest.
    reject_unreachable_core_nodes(&ctx.messenger, &peers).await?;

    let participants = reserve_participants(
        &ctx.messenger,
        ctx.bound_core_node.as_str(),
        ctx.core_instance_id.as_str(),
        &goal.launch_id,
        &slices,
        &ctx.peppy_version,
    )
    .await?;

    // Preflight is complete and every participant is reserved. Dispatch to
    // those participants (their add/build phases, and the wave-synchronized
    // instance starts) is NOT yet wired.
    //
    // Refuse here rather than falling through. The plan `planned` still holds
    // every deployment, including the peers', so continuing would start peer
    // instances on this daemon: the launch would report success while the
    // topology it produced is not the one the launcher describes. A loud
    // refusal after a clean, non-destructive preflight is the only honest
    // outcome, and it leaves every machine exactly as it was.
    release_participants(
        &ctx.messenger,
        ctx.bound_core_node.as_str(),
        ctx.core_instance_id.as_str(),
        &goal.launch_id,
        &participants
            .slices
            .iter()
            .map(|slice| slice.core_node.clone())
            .collect::<Vec<_>>(),
    )
    .await;

    Err(format!(
        "federated dispatch is not implemented yet, so this launch is refused rather than run          with {} instance(s) on the wrong machine. Preflight succeeded: {} reachable,          version-matched, and reserved. Nothing was torn down. Run the launcher on one machine          with `--local` in the meantime.",
        placements
            .participants()
            .len()
            .saturating_sub(1),
        participants
            .slices
            .iter()
            .map(|slice| format!("`{}`", slice.core_node))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}
