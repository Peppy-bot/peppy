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
//!
//! # One launch, one set of bytes
//!
//! The reservation carries the launch's pins: the coordinator resolved every
//! deployment, its transitive dependencies, and its contract and pairing
//! documents before this preflight ran, and each participant receives that
//! decision rather than a name to look up. A participant validates the pins
//! while the reservation is still non-destructive, and materializes them at
//! add time, reusing its own content on a fingerprint match and fetching the
//! pinned commit otherwise. Its own cache freshness, repository priorities
//! and exclusions never influence what this launch runs.

mod dispatch;

pub(super) use dispatch::{begin_participant_slices, clear_participant_slices, run_remote_goal};

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use core_node_api::encoding::{
    ParticipantReleaseRequest, ParticipantReserveRequest, ParticipantReserveResponse,
};
use daemon_config::format_quoted_list;
use daemon_config::launcher::{Deployment, Placements};
use daemon_config::repository::{DeploymentPins, PinnedItem};
use futures::future::join_all;
use peppylib::core_node::transport::poll;
use peppylib::{CoreNodePresenceMessenger, MessengerHandle};

/// Bound on one daemon-to-daemon preflight call. The peer validates decoded
/// pins in memory, so a healthy one answers well inside this; the budget
/// exists so an unreachable peer fails the launch instead of hanging it.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on a release. Shorter than a reserve because it does no validation
/// work, and it runs on the unwind path where waiting helps nobody.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(10);

/// What a coordinator learned from one participant during preflight.
#[derive(Debug, Clone)]
struct ParticipantSlice {
    core_node: String,
    /// This participant's root-entity instance id, folded into the
    /// coordinator's stack-wide instance-id uniqueness check.
    root_instance_id: String,
}

/// The pins each peer participant receives with its reservation, one
/// serialized [`DeploymentPins`] per pinned deployment with at least one
/// instance on it.
///
/// Every off-coordinator host of any instance appears as a key, because
/// every one of them must be reserved; a host whose share is only
/// content-addressed sources (`url:`) is reserved with no pins to validate.
/// The coordinator itself never appears: it holds no reservation on itself.
fn partition_reservations<'a>(
    items: impl Iterator<Item = (&'a Deployment, Option<&'a PinnedItem>, &'a [PinnedItem])>,
    placements: &Placements,
) -> std::result::Result<BTreeMap<String, Vec<String>>, String> {
    let coordinator = placements.coordinator();
    let mut by_core_node: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (deployment, root_pin, closure_pins) in items {
        let hosts: BTreeSet<&str> = deployment
            .instances
            .iter()
            .map(|instance| placements.of(instance.instance_id.as_str()))
            .filter(|host| *host != coordinator)
            .collect();
        // Built and serialized ONCE per deployment, not once per host: every
        // host of a deployment receives the same closure, and a closure holds
        // every dependency node plus every contract and pairing document.
        let encoded = match root_pin.filter(|_| !hosts.is_empty()) {
            Some(root) => {
                let pins = DeploymentPins::new(root.clone(), closure_pins.to_vec())?;
                Some(
                    serde_json5::to_string(&pins)
                        .map_err(|e| format!("could not encode a deployment's pins: {e}"))?,
                )
            }
            None => None,
        };
        for host in hosts {
            let entry = by_core_node.entry(host.to_owned()).or_default();
            if let Some(encoded) = &encoded {
                entry.push(encoded.clone());
            }
        }
    }
    Ok(by_core_node)
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

/// Reserves every peer participant, hands each its pins, and collects what
/// only they can report.
///
/// All-or-nothing: on any refusal the reservations already obtained are
/// released before the error returns, so a failed preflight leaves no machine
/// held. Only after a full set of acks does anything get torn down anywhere.
async fn reserve_participants(
    messenger: &MessengerHandle,
    coordinator: &str,
    caller_instance_id: &str,
    launch_id: &str,
    peers: BTreeMap<String, Vec<String>>,
    own_version: &str,
) -> std::result::Result<Vec<ParticipantSlice>, String> {
    let acks = join_all(peers.into_iter().map(|(core_node, pins)| {
        let request =
            ParticipantReserveRequest::new(launch_id, coordinator).with_deployment_pins(pins);
        async move {
            let outcome = poll(
                &request,
                messenger,
                coordinator,
                caller_instance_id,
                &core_node,
                PREFLIGHT_TIMEOUT,
            )
            .await
            .map_err(|e| format!("`{core_node}` did not answer the reservation: {e}"));
            (core_node, outcome)
        }
    }))
    .await;

    let mut slices: Vec<ParticipantSlice> = Vec::new();
    let mut reserved: Vec<String> = Vec::new();
    let mut refusals: Vec<String> = Vec::new();

    for (core_node, outcome) in acks {
        match outcome {
            Ok(response) if response.accepted => {
                reserved.push(core_node.clone());
                // The reservation is already recorded above, so a version
                // refusal here still releases it.
                match check_version(&core_node, &response, own_version) {
                    Ok(()) => slices.push(ParticipantSlice {
                        core_node,
                        root_instance_id: response.root_instance_id,
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
}

/// The whole preflight, in the order the plan requires.
///
/// Runs AFTER this daemon resolved every deployment (so the reservations can
/// carry the pins) and BEFORE the teardown that a launch performs, so every
/// refusal here leaves the coordinator's existing stack, and every
/// participant's, exactly as it was. That ordering is the feature: a launch
/// that cannot succeed must not cost you the stack you already had.
pub(super) async fn preflight(
    ctx: &super::ProcessLaunchContext,
    launch_id: &str,
    planned: &[super::PlannedDeployment],
    placements: &Placements,
) -> std::result::Result<FederatedLaunch, String> {
    if !placements.is_federated() {
        return Ok(FederatedLaunch::default());
    }

    let peers = partition_reservations(
        planned.iter().map(|item| {
            (
                &item.deployment,
                item.root_pin.as_ref(),
                item.closure_pins.as_slice(),
            )
        }),
        placements,
    )?;

    // A wired core node is validated by live zenoh presence, not the platform
    // HTTP roster: what a launch needs is to be able to talk to the machine
    // right now, which the roster does not attest.
    reject_unreachable_core_nodes(&ctx.messenger, &peers.keys().cloned().collect()).await?;

    let participants = reserve_participants(
        &ctx.messenger,
        ctx.bound_core_node.as_str(),
        ctx.core_instance_id.as_str(),
        launch_id,
        peers,
        &ctx.peppy_version,
    )
    .await?;

    Ok(FederatedLaunch { participants })
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use daemon_config::launcher::DeploymentInstance;
    use daemon_config::repository::{
        EntryOrigin, GitCommit, ItemName, ItemTag, ManifestFingerprint, PinKind, RepoRelativePath,
    };

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

    fn pin(name: &str) -> PinnedItem {
        PinnedItem {
            kind: PinKind::Node,
            name: ItemName::parse(name).expect("valid name"),
            tag: ItemTag::parse("v1").expect("valid tag"),
            sha256: ManifestFingerprint::parse(&"a".repeat(64)).expect("valid sha"),
            origin: EntryOrigin::Git {
                repo_url: "https://example.com/hub".to_owned(),
                repo_ref: Some("main".to_owned()),
                commit: GitCommit::parse(&"b".repeat(40)).expect("valid commit"),
                path: RepoRelativePath::parse(&format!("{name}/peppy.json5")).expect("valid path"),
            },
        }
    }

    fn core_node(name: &str) -> daemon_config::core_node_name::CoreNodeName {
        daemon_config::core_node_name::CoreNodeName::new(name).expect("valid test core node name")
    }

    fn placements(pairs: &[(&str, &str)]) -> Placements {
        Placements::new(
            core_node("cn-robot"),
            pairs
                .iter()
                .map(|(id, target)| ((*id).to_owned(), core_node(target)))
                .collect(),
        )
    }

    fn partition(
        items: &[(Deployment, Option<PinnedItem>, Vec<PinnedItem>)],
        placements: &Placements,
    ) -> BTreeMap<String, Vec<String>> {
        partition_reservations(
            items
                .iter()
                .map(|(deployment, root, closure)| (deployment, root.as_ref(), closure.as_slice())),
            placements,
        )
        .expect("partitions")
    }

    /// A single-machine launch reserves nobody: the coordinator holds no
    /// reservation on itself, so none of the federation machinery costs
    /// anything when nothing is placed elsewhere.
    #[test]
    fn a_single_machine_launch_reserves_no_peers() {
        let items = vec![(
            deployment("uvc_camera", vec![instance("cam_1")]),
            Some(pin("uvc_camera")),
            Vec::new(),
        )];
        let partitioned = partition(&items, &Placements::all_on(core_node("cn-robot")));
        assert!(partitioned.is_empty());
    }

    /// A peer receives the pins of exactly the deployments it hosts: the
    /// whole closure of each, and nothing about deployments elsewhere.
    #[test]
    fn a_peer_is_reserved_with_the_pins_of_its_own_deployments() {
        let items = vec![
            (
                deployment("uvc_camera", vec![instance("cam_1")]),
                Some(pin("uvc_camera")),
                Vec::new(),
            ),
            (
                deployment("planner", vec![instance("planner_1")]),
                Some(pin("planner")),
                vec![pin("planner_dep")],
            ),
        ];
        let partitioned = partition(&items, &placements(&[("planner_1", "cn-atlas")]));
        assert_eq!(
            partitioned.keys().cloned().collect::<Vec<_>>(),
            ["cn-atlas"]
        );
        assert_eq!(partitioned["cn-atlas"].len(), 1);
        let sent = &partitioned["cn-atlas"][0];
        assert!(sent.contains("planner"), "got: {sent}");
        assert!(sent.contains("planner_dep"), "got: {sent}");
        assert!(!sent.contains("uvc_camera"), "got: {sent}");
        // What crosses the wire decodes as the same pins that were sent.
        let decoded: DeploymentPins = serde_json5::from_str(sent).expect("round trips");
        assert_eq!(decoded.root.name, "planner");
        assert_eq!(decoded.closure.len(), 1);
    }

    /// A deployment whose instances straddle the coordinator and a peer
    /// sends the peer its pins too: both machines add from one decision.
    #[test]
    fn a_straddling_deployment_sends_its_pins_to_the_peer_half() {
        let items = vec![(
            deployment(
                "uvc_camera",
                vec![instance("cam_robot"), instance("cam_cloud")],
            ),
            Some(pin("uvc_camera")),
            Vec::new(),
        )];
        let partitioned = partition(&items, &placements(&[("cam_cloud", "cn-atlas")]));
        assert_eq!(
            partitioned.keys().cloned().collect::<Vec<_>>(),
            ["cn-atlas"]
        );
        assert_eq!(partitioned["cn-atlas"].len(), 1);
        assert!(partitioned["cn-atlas"][0].contains("uvc_camera"));
    }

    /// A host whose share is only content-addressed sources is still
    /// reserved: the reservation guards the machine, not the pins.
    #[test]
    fn a_peer_hosting_only_unpinned_deployments_is_still_reserved() {
        let items = vec![(
            deployment("recorder", vec![instance("rec_1")]),
            None,
            Vec::new(),
        )];
        let partitioned = partition(&items, &placements(&[("rec_1", "cn-atlas")]));
        assert_eq!(
            partitioned.keys().cloned().collect::<Vec<_>>(),
            ["cn-atlas"]
        );
        assert!(partitioned["cn-atlas"].is_empty());
    }

    #[test]
    fn a_version_mismatch_is_refused_and_names_both_versions() {
        let response = ParticipantReserveResponse::accepted("v0.19.0", "gen_1");
        let error =
            check_version("cn-atlas", &response, "v0.20.0").expect_err("skew must be refused");
        assert!(error.contains("v0.19.0"), "got: {error}");
        assert!(error.contains("v0.20.0"), "got: {error}");
        assert!(error.contains("same version"), "got: {error}");
    }

    #[test]
    fn a_matching_version_passes() {
        let response = ParticipantReserveResponse::accepted("v0.20.0", "gen_1");
        assert!(check_version("cn-atlas", &response, "v0.20.0").is_ok());
    }

    fn slice(core_node: &str) -> ParticipantSlice {
        ParticipantSlice {
            core_node: core_node.to_owned(),
            root_instance_id: format!("{core_node}_root"),
        }
    }

    /// A peer's root entity holds its instance id across launches, so it is
    /// part of that machine's namespace before this launch places anything
    /// there. The coordinator cannot see it without asking.
    #[test]
    fn an_instance_id_colliding_with_a_peers_root_entity_is_refused() {
        let federated = FederatedLaunch {
            participants: vec![slice("cn-atlas")],
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

    /// A single-machine launch has no participants, so none of this applies
    /// and none of it costs anything.
    #[test]
    fn a_single_machine_launch_refuses_nothing() {
        let federated = FederatedLaunch::default();
        assert!(federated.core_nodes().is_empty());
        assert!(
            federated
                .root_instance_collisions(&BTreeSet::from(["anything"]))
                .is_empty()
        );
    }
}
