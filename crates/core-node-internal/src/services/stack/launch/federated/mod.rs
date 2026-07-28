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

mod dispatch;

pub(super) use dispatch::{begin_participant_slices, clear_participant_slices, run_remote_goal};

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use core_node_api::encoding::{
    ParticipantReleaseRequest, ParticipantReserveRequest, ParticipantReserveResponse,
    ResolvedManifest,
};
use daemon_config::format_quoted_list;
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

/// One deployment as placed on one core node: only that machine's share of the
/// instances, plus where it sat in the coordinator's full deployment list.
///
/// The index is what lets a participant's answers be aligned back with the
/// plan. Without it a straddling deployment (instances on two machines) would
/// have no way to say "this manifest is for THAT entry".
#[derive(Debug, Clone)]
struct PlacedDeployment {
    index: usize,
    deployment: Deployment,
}

/// What a coordinator learned from one participant during preflight.
#[derive(Debug, Clone)]
struct ParticipantSlice {
    core_node: String,
    /// This participant's root-entity instance id, folded into the
    /// coordinator's stack-wide instance-id uniqueness check.
    root_instance_id: String,
    /// What this participant resolved, keyed by the deployment's index in the
    /// coordinator's plan. Keying on the index rather than pairing two
    /// position-aligned lists is what lets a straddling deployment say "this
    /// manifest is for THAT entry" without an alignment invariant to keep.
    manifests: BTreeMap<usize, ResolvedManifest>,
}

/// A manifest the coordinator did not resolve itself, and the participant that
/// did.
///
/// The coordinator VALIDATES against this. That is the point of delegating:
/// what it checks is provably what the participant will spawn, because the
/// participant read it from the same cache it will spawn from, rather than the
/// coordinator guessing from its own.
#[derive(Debug, Clone)]
pub(super) struct DelegatedManifest {
    pub(super) core_node: String,
    pub(super) config_json5: String,
}

/// Which deployments each participant hosts.
///
/// A deployment goes to every core node hosting at least one of its instances,
/// because that daemon must add and build the node before starting its share.
/// One node split across daemons is therefore added on both, which is exactly
/// what "several placed instances under one deployment" means operationally.
fn partition_deployments(
    deployments: &[Deployment],
    placements: &Placements,
    coordinator: &str,
) -> BTreeMap<String, Vec<PlacedDeployment>> {
    let mut by_core_node: BTreeMap<String, Vec<PlacedDeployment>> = BTreeMap::new();
    for (index, deployment) in deployments.iter().enumerate() {
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
                .push(PlacedDeployment {
                    index,
                    deployment: Deployment {
                        source: deployment.source.clone(),
                        instances,
                    },
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
async fn reject_unreachable_core_nodes(
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
        format_quoted_list(&missing),
        if live.is_empty() {
            "nothing".to_owned()
        } else {
            format_quoted_list(&live)
        }
    ))
}

/// Reserves every peer participant and collects what only they can report.
///
/// All-or-nothing: on any refusal the reservations already obtained are
/// released before the error returns, so a failed preflight leaves no machine
/// held. Only after a full set of acks does anything get torn down anywhere.
async fn reserve_participants(
    messenger: &MessengerHandle,
    coordinator: &str,
    caller_instance_id: &str,
    launch_id: &str,
    peers: BTreeMap<String, Vec<PlacedDeployment>>,
    own_version: &str,
) -> std::result::Result<Vec<ParticipantSlice>, String> {
    let acks = join_all(peers.into_iter().map(|(core_node, deployments)| {
        let request = build_reserve_request(launch_id, coordinator, &deployments);
        async move {
            let outcome = match request {
                Ok(request) => poll(
                    &request,
                    messenger,
                    coordinator,
                    caller_instance_id,
                    &core_node,
                    PREFLIGHT_TIMEOUT,
                )
                .await
                .map_err(|e| format!("`{core_node}` did not answer the reservation: {e}")),
                Err(reason) => Err(format!("`{core_node}`: {reason}")),
            };
            (core_node, deployments, outcome)
        }
    }))
    .await;

    let mut slices: Vec<ParticipantSlice> = Vec::new();
    let mut reserved: Vec<String> = Vec::new();
    let mut refusals: Vec<String> = Vec::new();

    for (core_node, deployments, outcome) in acks {
        match outcome {
            Ok(response) if response.accepted => {
                reserved.push(core_node.clone());
                match check_version(&core_node, &response, own_version) {
                    // The peer answers in the order it was asked, so zipping the
                    // requested deployments back onto the manifests is what
                    // recovers each one's index in the coordinator's plan.
                    Ok(()) => slices.push(ParticipantSlice {
                        core_node,
                        root_instance_id: response.root_instance_id,
                        manifests: deployments
                            .into_iter()
                            .map(|placed| placed.index)
                            .zip(response.manifests)
                            .collect(),
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
        return Ok(slices);
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
    deployments: &[PlacedDeployment],
) -> std::result::Result<ParticipantReserveRequest, String> {
    let mut sources = Vec::with_capacity(deployments.len());
    for placed in deployments {
        let deployment = &placed.deployment;
        if let DeploymentSource::Local(spec) = &deployment.source {
            return Err(super::resolve::local_source_refusal(spec));
        }
        sources.push(
            serde_json5::to_string(&deployment.source)
                .map_err(|e| format!("could not encode a deployment source: {e}"))?,
        );
    }
    Ok(ParticipantReserveRequest::new(launch_id, coordinator).with_deployment_sources(sources))
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
        match result {
            Ok(verdict) if verdict.ok => {}
            // A refusal is not a transport failure: the participant is holding
            // a reservation for a DIFFERENT launch, so this one never owned it
            // and the presence lease will not clear it either. Worth its own
            // line, because it means two coordinators overlapped.
            Ok(verdict) => tracing::warn!(
                "`{core_node}` refused to release launch `{launch_id}`: {}",
                verdict
                    .rejection_reason
                    .unwrap_or_else(|| "no reason given".to_owned())
            ),
            Err(e) => tracing::warn!(
                "could not release `{core_node}` from launch `{launch_id}` ({e}); its \
                 reservation drops on its own when this coordinator leaves the federation"
            ),
        }
    }))
    .await;
}

/// What a federated launch established before anything was torn down.
#[derive(Default)]
pub(super) struct FederatedLaunch {
    /// Empty for a single-machine launch.
    participants: Vec<ParticipantSlice>,
    /// Plan indices of the deployments the coordinator hosts at least one
    /// instance of. Needed to tell a wholly-remote deployment (whose manifest is
    /// delegated) from a straddling one (which the coordinator must resolve for
    /// itself anyway).
    coordinator_indices: BTreeSet<usize>,
}

/// The whole preflight, in the order the plan requires.
///
/// Runs BEFORE the teardown that a launch performs, so every refusal here
/// leaves the coordinator's existing stack, and every participant's, exactly as
/// it was. That ordering is the feature: a launch that cannot succeed must not
/// cost you the stack you already had.
pub(super) async fn preflight(
    ctx: &super::ProcessLaunchContext,
    launch_id: &str,
    deployments: &[Deployment],
    placements: &Placements,
) -> std::result::Result<FederatedLaunch, String> {
    if !placements.is_federated() {
        return Ok(FederatedLaunch::default());
    }

    let mut slices = partition_deployments(deployments, placements, ctx.bound_core_node.as_str());

    // Splitting the coordinator's own share out first leaves `slices` holding
    // exactly the peers, so each one's deployments can be moved into its
    // reservation rather than cloned.
    let coordinator_indices: BTreeSet<usize> = slices
        .remove(ctx.bound_core_node.as_str())
        .unwrap_or_default()
        .into_iter()
        .map(|placed| placed.index)
        .collect();

    // A wired core node is validated by live zenoh presence, not the platform
    // HTTP roster: what a launch needs is to be able to talk to the machine
    // right now, which the roster does not attest.
    reject_unreachable_core_nodes(&ctx.messenger, &slices.keys().cloned().collect()).await?;

    let participants = reserve_participants(
        &ctx.messenger,
        ctx.bound_core_node.as_str(),
        ctx.core_instance_id.as_str(),
        launch_id,
        slices,
        &ctx.peppy_version,
    )
    .await?;

    Ok(FederatedLaunch {
        participants,
        coordinator_indices,
    })
}

impl FederatedLaunch {
    /// Every peer taking part, in a stable order so failure messages and
    /// release fan-outs read the same way twice.
    pub(super) fn core_nodes(&self) -> Vec<String> {
        self.participants
            .iter()
            .map(|slice| slice.core_node.clone())
            .collect()
    }

    /// The manifests the coordinator will validate against instead of
    /// resolving them itself, keyed by deployment index.
    ///
    /// A deployment that straddles the coordinator and a peer is deliberately
    /// NOT delegated: the coordinator has to resolve it anyway to add it
    /// locally, so it resolves it and [`Self::disagreeing_manifests`] then
    /// checks that both machines read the same thing.
    pub(super) fn delegated_manifests(&self) -> BTreeMap<usize, DelegatedManifest> {
        let mut delegated = BTreeMap::new();
        for slice in &self.participants {
            for (index, manifest) in &slice.manifests {
                if self.coordinator_indices.contains(index) {
                    continue;
                }
                delegated.insert(
                    *index,
                    DelegatedManifest {
                        core_node: slice.core_node.clone(),
                        config_json5: manifest.config_json5.clone(),
                    },
                );
            }
        }
        delegated
    }

    /// Instance ids in the plan that collide with a participant's own root
    /// entity.
    ///
    /// Each daemon's root entity keeps its instance id across launches (the
    /// teardown preserves it), so it is part of that machine's namespace before
    /// this launch places anything there. The coordinator cannot see it without
    /// asking, which is why the reservation response carries it: a collision
    /// would otherwise surface as a confusing failure on the peer, after that
    /// machine's stack had already been replaced.
    pub(super) fn root_instance_collisions(
        &self,
        planned_instance_ids: &BTreeSet<&str>,
    ) -> Vec<String> {
        self.participants
            .iter()
            .filter(|slice| planned_instance_ids.contains(slice.root_instance_id.as_str()))
            .map(|slice| {
                format!(
                    "instance id `{}` is already the root entity of `{}`, which this launch \
                     places instances on. Rename the instance in the launcher.",
                    slice.root_instance_id, slice.core_node
                )
            })
            .collect()
    }

    /// The manifest hash a participant reported for the deployment at `index`,
    /// echoed onto every instance the coordinator dispatches to it.
    pub(super) fn manifest_sha256(&self, core_node: &str, index: usize) -> Option<&str> {
        self.participants
            .iter()
            .find(|slice| slice.core_node == core_node)?
            .manifests
            .get(&index)
            .map(|manifest| manifest.config_sha256.as_str())
    }

    /// Straddling deployments whose two machines resolved DIFFERENT manifests.
    ///
    /// One node running on two machines under one launcher entry must be the
    /// same node on both, or the graph the coordinator validated describes
    /// neither. Two caches that have drifted is exactly how that happens, and
    /// it is silent unless something compares them.
    pub(super) fn disagreeing_manifests(
        &self,
        coordinator: &str,
        own_fingerprints: &BTreeMap<usize, String>,
    ) -> Vec<String> {
        let mut disagreements = Vec::new();
        for slice in &self.participants {
            for (index, manifest) in &slice.manifests {
                let Some(own) = own_fingerprints.get(index) else {
                    continue;
                };
                if own == &manifest.config_sha256 {
                    continue;
                }
                disagreements.push(format!(
                    "one deployment places instances on both `{coordinator}` and `{}`, but the \
                     two resolve different manifests for it ({own} here, {} there). Refresh both \
                     machines' caches so they agree, or split the deployment.",
                    slice.core_node, manifest.config_sha256
                ));
            }
        }
        disagreements
    }
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

    fn placed(index: usize, deployment: Deployment) -> PlacedDeployment {
        PlacedDeployment { index, deployment }
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
        let partitioned =
            partition_deployments(&deployments, &Placements::all_on("cn-robot"), "cn-robot");
        assert_eq!(partitioned.len(), 1);
        assert_eq!(partitioned["cn-robot"].len(), 1);
        assert_eq!(partitioned["cn-robot"][0].index, 0);
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
        assert_eq!(
            partitioned["cn-robot"][0].deployment.instances[0]
                .instance_id
                .as_str(),
            "cam_1"
        );
        // The index is what lets a participant's answers be matched back to
        // the coordinator's plan, so it must survive partitioning.
        assert_eq!(partitioned["cn-robot"][0].index, 0);
        assert_eq!(
            partitioned["cn-atlas"][0].deployment.instances[0]
                .instance_id
                .as_str(),
            "planner_1"
        );
        assert_eq!(partitioned["cn-atlas"][0].index, 1);
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
        assert_eq!(partitioned["cn-robot"][0].deployment.instances.len(), 1);
        assert_eq!(
            partitioned["cn-robot"][0].deployment.instances[0]
                .instance_id
                .as_str(),
            "cam_robot"
        );

        assert_eq!(partitioned["cn-atlas"].len(), 1);
        assert_eq!(partitioned["cn-atlas"][0].deployment.instances.len(), 1);
        assert_eq!(
            partitioned["cn-atlas"][0].deployment.instances[0]
                .instance_id
                .as_str(),
            "cam_cloud"
        );
        // Both halves point at the SAME entry in the coordinator's plan, which
        // is what makes the straddle detectable at all.
        assert_eq!(partitioned["cn-robot"][0].index, 0);
        assert_eq!(partitioned["cn-atlas"][0].index, 0);
    }

    #[test]
    fn a_local_source_placed_off_coordinator_is_refused_before_dispatch() {
        let local = Deployment {
            source: serde_json5::from_str(r#"{ local: "./nodes/planner" }"#)
                .expect("valid local source"),
            instances: vec![instance("planner_1")],
        };
        let error = build_reserve_request("launch-a", "cn-robot", &[placed(0, local)])
            .expect_err("a local source cannot cross machines");
        assert!(error.contains("names a tree on"), "got: {error}");
        assert!(error.contains("repo or url source"), "got: {error}");
    }

    #[test]
    fn a_host_independent_source_is_sent_verbatim_for_the_peer_to_resolve() {
        let request = build_reserve_request(
            "launch-a",
            "cn-robot",
            &[placed(
                0,
                deployment("deliberative_planner", vec![instance("planner_1")]),
            )],
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

    // --- What the coordinator does with what preflight learned ---

    fn slice(core_node: &str, entries: &[(usize, &str, &str)]) -> ParticipantSlice {
        ParticipantSlice {
            core_node: core_node.to_owned(),
            root_instance_id: format!("{core_node}_root"),
            manifests: entries
                .iter()
                .map(|(index, config, sha)| (*index, ResolvedManifest::new(*config, *sha)))
                .collect(),
        }
    }

    /// A deployment nothing local hosts is validated against the manifest the
    /// participant read, not one this daemon went and fetched. That is what
    /// makes the coordinator independent of sources it does not use.
    #[test]
    fn a_wholly_remote_deployment_takes_its_peers_manifest() {
        let federated = FederatedLaunch {
            participants: vec![slice("cn-atlas", &[(1, "{ manifest: {} }", "sha-cloud")])],
            coordinator_indices: BTreeSet::from([0]),
        };

        let delegated = federated.delegated_manifests();
        assert_eq!(
            delegated.len(),
            1,
            "only the remote deployment is delegated"
        );
        assert_eq!(delegated[&1].core_node, "cn-atlas");
        assert_eq!(delegated[&1].config_json5, "{ manifest: {} }");
        assert!(
            !delegated.contains_key(&0),
            "a deployment this daemon hosts must be resolved here, not taken on trust"
        );
    }

    /// A straddling deployment is NOT delegated: the coordinator has to resolve
    /// it anyway to add it locally, so it resolves it and the answers are then
    /// compared.
    #[test]
    fn a_straddling_deployment_is_resolved_locally_rather_than_delegated() {
        let federated = FederatedLaunch {
            participants: vec![slice("cn-atlas", &[(0, "{ manifest: {} }", "sha-cloud")])],
            coordinator_indices: BTreeSet::from([0]),
        };
        assert!(
            federated.delegated_manifests().is_empty(),
            "a deployment with a local half must be read locally"
        );
    }

    /// The silent failure this check exists to stop: one launcher entry, two
    /// machines, two caches that have drifted, and a validated graph that
    /// describes neither of the nodes actually running.
    #[test]
    fn two_machines_resolving_one_node_differently_is_refused() {
        let federated = FederatedLaunch {
            participants: vec![slice("cn-atlas", &[(0, "{ manifest: {} }", "sha-cloud")])],
            coordinator_indices: BTreeSet::from([0]),
        };

        let own = BTreeMap::from([(0, "sha-robot".to_owned())]);
        let disagreements = federated.disagreeing_manifests("cn-robot", &own);
        assert_eq!(disagreements.len(), 1);
        assert!(
            disagreements[0].contains("sha-robot"),
            "got: {disagreements:?}"
        );
        assert!(
            disagreements[0].contains("sha-cloud"),
            "got: {disagreements:?}"
        );
        assert!(
            disagreements[0].contains("cn-atlas"),
            "got: {disagreements:?}"
        );
    }

    #[test]
    fn two_machines_resolving_one_node_identically_is_accepted() {
        let federated = FederatedLaunch {
            participants: vec![slice("cn-atlas", &[(0, "{ manifest: {} }", "sha-same")])],
            coordinator_indices: BTreeSet::from([0]),
        };
        let own = BTreeMap::from([(0, "sha-same".to_owned())]);
        assert!(federated.disagreeing_manifests("cn-robot", &own).is_empty());
    }

    /// A delegated deployment contributes no local fingerprint, so it must not
    /// be compared: comparing a peer's answer against itself would pass
    /// vacuously and make the straddle check look like it was working.
    #[test]
    fn a_delegated_deployment_is_not_compared_against_itself() {
        let federated = FederatedLaunch {
            participants: vec![slice("cn-atlas", &[(1, "{ manifest: {} }", "sha-cloud")])],
            coordinator_indices: BTreeSet::from([0]),
        };
        // Index 1 is absent from the local fingerprints: this daemon never read it.
        let own = BTreeMap::from([(0, "sha-robot".to_owned())]);
        assert!(federated.disagreeing_manifests("cn-robot", &own).is_empty());
    }

    /// The hash echoed onto a dispatched start, which the peer re-checks
    /// against its own cache before spawning.
    #[test]
    fn the_dispatched_start_pins_the_hash_its_participant_reported() {
        let federated = FederatedLaunch {
            participants: vec![slice(
                "cn-atlas",
                &[(1, "{ a: 1 }", "sha-a"), (2, "{ b: 2 }", "sha-b")],
            )],
            coordinator_indices: BTreeSet::new(),
        };
        assert_eq!(federated.manifest_sha256("cn-atlas", 2), Some("sha-b"));
        assert_eq!(federated.manifest_sha256("cn-atlas", 1), Some("sha-a"));
        assert_eq!(
            federated.manifest_sha256("cn-atlas", 9),
            None,
            "a deployment that participant does not host pins nothing"
        );
        assert_eq!(federated.manifest_sha256("cn-elsewhere", 1), None);
    }

    /// A peer's root entity holds its instance id across launches, so it is
    /// part of that machine's namespace before this launch places anything
    /// there. The coordinator cannot see it without asking.
    #[test]
    fn an_instance_id_colliding_with_a_peers_root_entity_is_refused() {
        let federated = FederatedLaunch {
            participants: vec![slice("cn-atlas", &[])],
            coordinator_indices: BTreeSet::new(),
        };

        let collisions =
            federated.root_instance_collisions(&BTreeSet::from(["cn-atlas_root", "planner_inst"]));
        assert_eq!(collisions.len(), 1);
        assert!(
            collisions[0].contains("cn-atlas_root"),
            "got: {collisions:?}"
        );
        assert!(collisions[0].contains("cn-atlas"), "got: {collisions:?}");

        assert!(
            federated
                .root_instance_collisions(&BTreeSet::from(["planner_inst"]))
                .is_empty()
        );
    }

    /// A single-machine launch has no participants, so none of this applies and
    /// none of it costs anything.
    #[test]
    fn a_single_machine_launch_delegates_nothing_and_refuses_nothing() {
        let federated = FederatedLaunch::default();
        assert!(federated.core_nodes().is_empty());
        assert!(federated.delegated_manifests().is_empty());
        assert!(
            federated
                .disagreeing_manifests("cn-robot", &BTreeMap::from([(0, "sha".to_owned())]))
                .is_empty()
        );
        assert!(
            federated
                .root_instance_collisions(&BTreeSet::from(["anything"]))
                .is_empty()
        );
    }
}
