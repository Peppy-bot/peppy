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
use daemon_config::repository::{DeploymentPins, DeploymentRoot, PinnedItem};
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

/// What a launch asks of every machine's clock.
///
/// Simulated time is the operator's choice, made per machine with
/// `peppy service serve --clock-source`, so the launch reads it off the
/// coordinator rather than inventing it: a launch typed at a sim-mode daemon
/// is a simulated launch, and every machine it spans must serve simulated
/// time too. An instance that names `use_sim_time: true` outright asks for it
/// wherever it lands, so it commits the launch on its own.
///
/// A declared time source does NOT commit the launch. The same launcher
/// describes a robot whether or not the operator started the daemon in sim
/// mode; on a wall-mode machine the declaration resolves to no participants
/// and the source publishes nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ClockDemand {
    Wall,
    Sim(SimDemandOrigin),
    /// The coordinator hosts none of the launch and no instance forces a
    /// clock, so the machines that do host it decide: they must agree among
    /// themselves, checked when their reservations come back
    /// ([`check_hosts_agree`]). The coordinator's own clock stays out of it;
    /// a machine running none of the launch is not its business.
    HostsDecide,
}

/// What committed a launch to simulated time, carried in [`ClockDemand::Sim`]
/// and rendered into the clock refusals so the operator sees every fix, not
/// only the restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SimDemandOrigin {
    /// The coordinating daemon serves simulated time, so every launch typed
    /// at it is simulated.
    CoordinatorClock,
    /// The named instance sets `framework: { use_sim_time: true }`, which
    /// commits the launch wherever it lands.
    Instance(String),
}

impl SimDemandOrigin {
    /// The refusal clause naming what committed the launch.
    fn because(&self) -> String {
        match self {
            Self::CoordinatorClock => "because the coordinating daemon serves it".to_owned(),
            Self::Instance(instance_id) => {
                format!("because instance `{instance_id}` sets `use_sim_time: true`")
            }
        }
    }

    /// The refusal's alternative remedy, where one exists.
    fn alternative(&self) -> String {
        match self {
            Self::CoordinatorClock => String::new(),
            Self::Instance(instance_id) => {
                format!(", or drop `{instance_id}`'s `use_sim_time` override")
            }
        }
    }
}

impl ClockDemand {
    pub(super) fn of<'a>(
        instances: impl IntoIterator<Item = &'a daemon_config::launcher::DeploymentInstance>,
        coordinator_serves_sim_time: bool,
        coordinator_hosts: bool,
    ) -> Self {
        if coordinator_hosts && coordinator_serves_sim_time {
            return Self::Sim(SimDemandOrigin::CoordinatorClock);
        }
        if let Some(reader) = instances
            .into_iter()
            .find(|instance| instance.framework.use_sim_time == Some(true))
        {
            return Self::Sim(SimDemandOrigin::Instance(reader.instance_id.to_string()));
        }
        if coordinator_hosts {
            return Self::Wall;
        }
        Self::HostsDecide
    }
}

/// The pins each peer participant receives with its reservation, one
/// serialized [`DeploymentPins`] per pinned deployment with at least one
/// instance on it.
///
/// Every off-coordinator host of any instance appears as a key, because
/// every one of them must be reserved. The coordinator itself never
/// appears: it holds no reservation on itself.
fn partition_reservations<'a>(
    items: impl Iterator<Item = (&'a Deployment, &'a DeploymentRoot, &'a [PinnedItem])>,
    placements: &Placements,
) -> std::result::Result<BTreeMap<String, Vec<String>>, String> {
    let coordinator = placements.coordinator();
    let mut by_core_node: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (deployment, root, closure_pins) in items {
        let hosts: BTreeSet<&str> = deployment
            .instances
            .iter()
            .map(|instance| placements.of(instance.instance_id.as_str()))
            .filter(|host| *host != coordinator)
            .collect();
        if hosts.is_empty() {
            continue;
        }
        // Built and serialized ONCE per deployment, not once per host: every
        // host of a deployment receives the same closure, and a closure holds
        // every dependency node plus every contract and pairing document.
        let pins = DeploymentPins::new(root.clone(), closure_pins.to_vec())?;
        let encoded = serde_json5::to_string(&pins)
            .map_err(|e| format!("could not encode a deployment's pins: {e}"))?;
        for host in hosts {
            by_core_node
                .entry(host.to_owned())
                .or_default()
                .push(encoded.clone());
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
    clock_demand: &ClockDemand,
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
    let mut to_release: Vec<String> = Vec::new();
    let mut refusals: Vec<String> = Vec::new();

    let mut host_clocks: Vec<(String, bool)> = Vec::new();
    for (core_node, outcome) in acks {
        match outcome {
            Ok(response) if response.accepted => {
                to_release.push(core_node.clone());
                // The reservation is already recorded above, so a version or
                // clock refusal here still releases it.
                match check_version(&core_node, &response, own_version)
                    .and_then(|()| check_clock_source(&core_node, &response, clock_demand))
                {
                    Ok(()) => {
                        host_clocks.push((core_node.clone(), response.serves_sim_time));
                        slices.push(ParticipantSlice {
                            core_node,
                            root_instance_id: response.root_instance_id,
                        });
                    }
                    Err(reason) => refusals.push(reason),
                }
            }
            // An answered refusal reserved nothing, so the peer owes no
            // release.
            Ok(response) => refusals.push(format!(
                "`{core_node}` refused: {}",
                response
                    .rejection_reason
                    .unwrap_or_else(|| "no reason given".to_owned())
            )),
            // An unanswered reservation is not a refusal: the request may have
            // landed and reserved the peer with only the ack lost, so it is
            // released with the ones that acked. Releasing a peer that never
            // reserved succeeds as a no-op.
            Err(reason) => {
                to_release.push(core_node.clone());
                refusals.push(reason);
            }
        }
    }

    if refusals.is_empty()
        && matches!(clock_demand, ClockDemand::HostsDecide)
        && let Err(reason) = check_hosts_agree(&host_clocks)
    {
        refusals.push(reason);
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
        &to_release,
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

/// Every machine of a launch must serve the same kind of time, so this
/// refuses a peer whose clock source disagrees with the launch, in either
/// direction. Refused before any stack is touched, naming the machine and the
/// flag that fixes it.
fn check_clock_source(
    core_node: &str,
    response: &ParticipantReserveResponse,
    clock_demand: &ClockDemand,
) -> std::result::Result<(), String> {
    check_clock_agreement(core_node, response.serves_sim_time, clock_demand)
}

/// The one clock-agreement refusal, applied to a peer from its reservation
/// and to the coordinator from its own defaults.
///
/// Both disagreements are real failures, not tidiness. A wall-mode machine in
/// a simulated launch publishes its own ticks onto the very key the sim-time
/// instances there read, so their time alternates. A sim-mode machine in a
/// wall launch is the mirror: every instance placed there resolves to
/// simulated time that nothing in this launch publishes, so each one waits at
/// "clock not ready" for as long as it runs.
pub(super) fn check_clock_agreement(
    core_node: &str,
    serves_sim_time: bool,
    clock_demand: &ClockDemand,
) -> std::result::Result<(), String> {
    match (clock_demand, serves_sim_time) {
        // The machines of a HostsDecide launch are held to each other, not to
        // a demand, in [`check_hosts_agree`] once every reservation is in.
        (ClockDemand::HostsDecide, _) => Ok(()),
        (ClockDemand::Sim(_), true) | (ClockDemand::Wall, false) => Ok(()),
        (ClockDemand::Sim(origin), false) => Err(format!(
            "`{core_node}` serves wall time but this launch runs on simulated time, {because}. \
             Every machine of a simulated launch must serve it, because a wall-mode daemon \
             publishes its own ticks onto the key its sim-time instances read: restart \
             `{core_node}` with `peppy service serve --clock-source=sim`{alternative}.",
            because = origin.because(),
            alternative = origin.alternative(),
        )),
        (ClockDemand::Wall, true) => Err(format!(
            "`{core_node}` serves simulated time but this launch runs on wall time. Every \
             instance placed there would resolve to simulated time that nothing in this launch \
             publishes, and wait at `clock not ready`: restart `{core_node}` with `peppy \
             service serve` to serve wall time, or launch from a daemon started with \
             `--clock-source=sim`."
        )),
    }
}

/// A fully-placed launch takes its clock from the machines that host it,
/// which must agree among themselves, in either mode. Unanimous simulated
/// hosts run the launch on the declared source's ticks; unanimous wall hosts
/// run it on their own; a split is refused naming one machine of each side,
/// with every reservation released.
pub(super) fn check_hosts_agree(hosts: &[(String, bool)]) -> std::result::Result<(), String> {
    let sim_machine = hosts.iter().find(|(_, serves)| *serves);
    let wall_machine = hosts.iter().find(|(_, serves)| !*serves);
    match (sim_machine, wall_machine) {
        (Some((sim_machine, _)), Some((wall_machine, _))) => Err(format!(
            "the machines of this launch disagree about the clock: `{sim_machine}` serves \
             simulated time while `{wall_machine}` serves wall time. Every machine of a launch \
             serves the same kind of time; restart one side until they agree (`peppy service \
             serve --clock-source=sim` for simulated, plain `peppy service serve` for wall)."
        )),
        _ => Ok(()),
    }
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
                 reservation drops on its own when this coordinator reserves it for its \
                 next launch or leaves the federation"
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
    clock_demand: &ClockDemand,
) -> std::result::Result<FederatedLaunch, String> {
    if !placements.is_federated() {
        return Ok(FederatedLaunch::default());
    }

    let peers = partition_reservations(
        planned
            .iter()
            .map(|item| (&item.deployment, &item.root, item.closure_pins.as_slice())),
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
        clock_demand,
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
    // The one spelling of a git-backed node pin, defined in the parent
    // module's tests so this module and `launch.rs` cannot drift apart.
    use super::super::tests::test_root_pin as pin;
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
        items: &[(Deployment, PinnedItem, Vec<PinnedItem>)],
        placements: &Placements,
    ) -> BTreeMap<String, Vec<String>> {
        let roots: Vec<DeploymentRoot> = items
            .iter()
            .map(|(_, root, _)| DeploymentRoot::Node(root.clone()))
            .collect();
        partition_reservations(
            items
                .iter()
                .zip(&roots)
                .map(|((deployment, _, closure), root)| (deployment, root, closure.as_slice())),
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
            pin("uvc_camera"),
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
                pin("uvc_camera"),
                Vec::new(),
            ),
            (
                deployment("planner", vec![instance("planner_1")]),
                pin("planner"),
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
        assert_eq!(decoded.root.label(), "node `planner:v1`");
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
            pin("uvc_camera"),
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

    #[test]
    fn a_version_mismatch_is_refused_and_names_both_versions() {
        let response = ParticipantReserveResponse::accepted("v0.19.0", "gen_1", false);
        let error =
            check_version("cn-atlas", &response, "v0.20.0").expect_err("skew must be refused");
        assert!(error.contains("v0.19.0"), "got: {error}");
        assert!(error.contains("v0.20.0"), "got: {error}");
        assert!(error.contains("same version"), "got: {error}");
    }

    #[test]
    fn a_matching_version_passes() {
        let response = ParticipantReserveResponse::accepted("v0.20.0", "gen_1", false);
        assert!(check_version("cn-atlas", &response, "v0.20.0").is_ok());
    }

    /// A peer must serve the same kind of time the launch runs on, in either
    /// direction, and each refusal names the machine and the way out. The
    /// mirror case matters as much as the obvious one: a sim-mode peer in a
    /// wall launch strands every instance placed there at "clock not ready".
    #[test]
    fn a_peer_whose_clock_disagrees_with_the_launch_is_refused() {
        let wall_peer = ParticipantReserveResponse::accepted("v0.20.0", "gen_1", false);
        let sim_peer = ParticipantReserveResponse::accepted("v0.20.0", "gen_1", true);

        let coordinator_sim = ClockDemand::Sim(SimDemandOrigin::CoordinatorClock);
        assert!(check_clock_source("cn-atlas", &wall_peer, &ClockDemand::Wall).is_ok());
        assert!(check_clock_source("cn-atlas", &sim_peer, &coordinator_sim).is_ok());

        let wall_in_sim = check_clock_source("cn-atlas", &wall_peer, &coordinator_sim)
            .expect_err("a wall peer cannot host simulated time");
        assert!(wall_in_sim.contains("`cn-atlas`"), "got: {wall_in_sim}");
        assert!(
            wall_in_sim.contains("--clock-source=sim"),
            "got: {wall_in_sim}"
        );
        assert!(
            wall_in_sim.contains("coordinating daemon serves it"),
            "the refusal says what committed the launch: {wall_in_sim}"
        );

        // Committed by an instance instead: the refusal names it and offers
        // dropping its override as the other way out.
        let reader_sim = ClockDemand::Sim(SimDemandOrigin::Instance("relay".to_owned()));
        let by_reader = check_clock_source("cn-atlas", &wall_peer, &reader_sim)
            .expect_err("a wall peer cannot host simulated time");
        assert!(
            by_reader.contains("instance `relay` sets `use_sim_time: true`"),
            "got: {by_reader}"
        );
        assert!(
            by_reader.contains("drop `relay`'s `use_sim_time` override"),
            "got: {by_reader}"
        );

        let sim_in_wall = check_clock_source("cn-atlas", &sim_peer, &ClockDemand::Wall)
            .expect_err("a sim peer strands a wall launch's instances");
        assert!(sim_in_wall.contains("`cn-atlas`"), "got: {sim_in_wall}");
        assert!(
            sim_in_wall.contains("clock not ready"),
            "got: {sim_in_wall}"
        );
    }

    /// A launch runs on simulated time because the operator started the
    /// coordinator in sim mode, or because an instance asks for it outright.
    /// Declaring a time source does not: that same launcher has to keep
    /// working against a wall-mode daemon, where it publishes nothing.
    #[test]
    fn the_clock_demand_follows_the_coordinator_and_explicit_readers() {
        let parse = |json5: &str| -> DeploymentInstance {
            serde_json5::from_str(json5).expect("instance fixture should parse")
        };
        let wall = parse(r#"{ instance_id: "arm" }"#);
        let forced_wall = parse(r#"{ instance_id: "cam", framework: { use_sim_time: false } }"#);
        let reader = parse(r#"{ instance_id: "relay", framework: { use_sim_time: true } }"#);
        let source = parse(r#"{ instance_id: "sim", framework: { publishes_sim_time: true } }"#);

        assert_eq!(
            ClockDemand::of([&wall, &forced_wall], false, true),
            ClockDemand::Wall
        );
        assert_eq!(
            ClockDemand::of([&wall, &reader], false, true),
            ClockDemand::Sim(SimDemandOrigin::Instance("relay".to_owned())),
            "the demand carries which instance committed the launch"
        );
        assert_eq!(
            ClockDemand::of([&source], false, true),
            ClockDemand::Wall,
            "a declared source alone leaves a wall-mode launch on wall time"
        );
        assert_eq!(
            ClockDemand::of([&wall], true, true),
            ClockDemand::Sim(SimDemandOrigin::CoordinatorClock)
        );
        assert_eq!(ClockDemand::of([], false, true), ClockDemand::Wall);

        // A coordinator hosting none of the launch does not get a vote: the
        // hosting machines decide, unless an instance forces the clock, which
        // commits the launch wherever it lands.
        assert_eq!(
            ClockDemand::of([&wall], false, false),
            ClockDemand::HostsDecide
        );
        assert_eq!(
            ClockDemand::of([&wall], true, false),
            ClockDemand::HostsDecide,
            "a non-hosting sim workstation cannot commit a launch to its clock"
        );
        assert_eq!(
            ClockDemand::of([&wall, &reader], false, false),
            ClockDemand::Sim(SimDemandOrigin::Instance("relay".to_owned()))
        );
    }

    /// The machines of a fully-placed launch are held to each other: any
    /// unanimous clock passes, a split is refused naming one machine of each
    /// side.
    #[test]
    fn the_hosts_of_a_fully_placed_launch_must_agree_with_each_other() {
        let unanimous_sim = [("cn-a".to_owned(), true), ("cn-b".to_owned(), true)];
        let unanimous_wall = [("cn-a".to_owned(), false), ("cn-b".to_owned(), false)];
        assert!(check_hosts_agree(&unanimous_sim).is_ok());
        assert!(check_hosts_agree(&unanimous_wall).is_ok());
        assert!(check_hosts_agree(&[]).is_ok());

        let split = [("cn-sim".to_owned(), true), ("cn-wall".to_owned(), false)];
        let refusal = check_hosts_agree(&split).expect_err("a split fleet is refused");
        assert!(
            refusal.contains("`cn-sim`") && refusal.contains("`cn-wall`"),
            "one machine of each side is named: {refusal}"
        );
        assert!(
            refusal.contains("--clock-source=sim"),
            "the refusal says how to move either way: {refusal}"
        );
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
