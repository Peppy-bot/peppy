//! The daemon-to-daemon endpoints of a federated launch: reserve, begin the
//! slice, release, and the runtime relationship notification.
//!
//! Reserving and beginning the slice are deliberately two calls. Reserving is
//! non-destructive and happens before the coordinator knows whether every
//! participant will accept; beginning the slice is the destructive commit, sent
//! only once they all have. Collapsing them would replace a stack on one
//! machine for a launch another machine is about to refuse.
//!
//! The reservation handler is where the lease is wired: accepting a
//! reservation also spawns a watch on the coordinator's core-node presence, so
//! [`SliceOwnership::release_because_coordinator_gone`] fires the moment the
//! coordinator leaves the federation. The state machine itself lives in the
//! parent module and is tested there without any of this plumbing.

use super::{Lease, ReserveOutcome, SliceOwnership};
use crate::Result;
use crate::services::node::RelationshipCoordinators;
use crate::services::response::into_service_response;
use core_node_api::ServiceId;
use core_node_api::encoding::{
    FederationVerdict, IncarnationEntry, IncarnationQueryRequest, IncarnationQueryResponse,
    LaunchIdentity, PairCommitRequest, ParticipantReleaseRequest, ParticipantReserveRequest,
    ParticipantReserveResponse, ParticipantSliceBeginRequest, ParticipantSliceBeginResponse,
    RelationshipEvent, RelationshipNotification, RelationshipNotificationAck,
};
use core_node_api::names;
use daemon_config::repository::DeploymentPins;
use peppylib::messaging::{SenderTarget, ServiceRequestContext};
use peppylib::types::Payload;
use peppylib::{CoreNodePresenceMessenger, LivelinessEvent, MessengerHandle, ServiceMessenger};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Everything the federation endpoints need from the daemon.
#[derive(Clone)]
pub(crate) struct FederationServiceContext {
    pub(crate) messenger: MessengerHandle,
    pub(crate) core_node_name: String,
    pub(crate) ownership: Arc<SliceOwnership>,
    pub(crate) relationships: RelationshipCoordinators,
    /// This daemon's stack, so `participant_slice_begin` can replace the slice
    /// the coordinator is about to repopulate.
    pub(crate) node_stack: Arc<node_stack::NodeStack>,
    /// This daemon's version string, the same one the `info` service reports,
    /// so a coordinator comparing versions has exactly one source of truth.
    pub(crate) peppy_version: String,
    /// The daemon's own root-entity instance id, folded into the coordinator's
    /// global instance-id uniqueness check.
    pub(crate) root_instance_id: String,
    /// Cancelled on daemon shutdown so a coordinator-presence watch does not
    /// outlive the daemon generation that spawned it.
    pub(crate) shutdown_token: CancellationToken,
}

/// Declares the listener for one federation endpoint.
///
/// The five endpoints differ only in which [`ServiceId`] they bind and which
/// handler they run; binding, spawning, and turning the handler's `Result` into
/// a service response are identical for all of them. Stating that once is what
/// stops a change to how these endpoints are served from landing on four of the
/// five.
macro_rules! federation_endpoint {
    ($listen:ident, $service:expr, $inner:ident) => {
        pub(crate) async fn $listen(
            context: FederationServiceContext,
            node_name: &str,
        ) -> Result<JoinHandle<Result<()>>> {
            let mut endpoint = ServiceMessenger::listen(
                &context.messenger,
                &context.core_node_name,
                &context.root_instance_id,
                SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
                $service.name(),
            )
            .await?;

            Ok(tokio::spawn(async move {
                endpoint
                    .handle_requests(|request| {
                        let context = context.clone();
                        async move {
                            into_service_response(&request, $inner(&request, &context).await)
                        }
                    })
                    .await
                    .map_err(Into::into)
            }))
        }
    };
}

federation_endpoint!(
    listen_for_participant_reserve,
    ServiceId::ParticipantReserve,
    reserve_inner
);
federation_endpoint!(
    listen_for_participant_slice_begin,
    ServiceId::ParticipantSliceBegin,
    slice_begin_inner
);
federation_endpoint!(
    listen_for_pair_commit,
    ServiceId::PairCommit,
    pair_commit_inner
);
federation_endpoint!(
    listen_for_participant_release,
    ServiceId::ParticipantRelease,
    release_inner
);
federation_endpoint!(
    listen_for_relationship_notify,
    ServiceId::RelationshipNotify,
    notify_inner
);
federation_endpoint!(
    listen_for_incarnation_query,
    ServiceId::IncarnationQuery,
    incarnation_query_inner
);

async fn reserve_inner(
    request: &ServiceRequestContext,
    context: &FederationServiceContext,
) -> Result<Payload> {
    let decoded = ParticipantReserveRequest::decode(request.message().payload_bytes().as_ref())?;

    debug!(
        "Received `participant_reserve` for launch `{}` from coordinator `{}`",
        decoded.launch_id, decoded.coordinator_core_node
    );

    // Validate the pins the coordinator resolved for this slice BEFORE taking
    // the reservation. Decoding runs every structural rule (a traversal path,
    // a truncated commit, a malformed fingerprint all fail there), and the
    // origin check refuses a pin no machine but the coordinator could read.
    // Nothing here touches the filesystem, the network, or this daemon's
    // caches: the bytes are materialized at add time, and which bytes those
    // are was decided on the coordinator. Refusing first means a rejected
    // slice never took ownership and never spawned a presence watch, so there
    // is nothing to unwind.
    if let Err(reason) = validate_deployment_pins(&decoded.deployment_pins_json5) {
        return ParticipantReserveResponse::rejected(reason, &context.peppy_version)
            .encode()
            .map_err(Into::into);
    }

    match context
        .ownership
        .try_reserve(&decoded.launch_id, &decoded.coordinator_core_node)
    {
        ReserveOutcome::HeldByAnotherLaunch {
            launch_id,
            coordinator_core_node,
        } => {
            // Refuse before resolving anything. The coordinator releases
            // whatever it did obtain and fails the launch, so no machine is
            // left with a half-replaced stack by a launch that never had a
            // chance. The remedy names THIS machine: a bare `stack reset`
            // targets whichever daemon the operator's CLI connects to, which
            // is not the one holding the reservation.
            return ParticipantReserveResponse::rejected(
                format!(
                    "already reserved for launch `{launch_id}` driven by core node \
                     `{coordinator_core_node}`. If that launch is no longer running, clear \
                     this machine with `peppy stack reset --core-node {}`",
                    context.core_node_name
                ),
                &context.peppy_version,
            )
            .encode()
            .map_err(Into::into);
        }
        // Only a FRESH reservation needs a watch, and the lease it hands back
        // is what that watch supervises. A coordinator retrying a dropped reply
        // re-reserves what it already holds, and a takeover moves the launch id
        // inside the reservation already held: in both cases the live lease is
        // the one the existing watch is holding, so watching again would leave
        // a presence subscription per retry.
        ReserveOutcome::Reserved { lease } => {
            watch_coordinator_presence(context, &decoded.coordinator_core_node, lease)
        }
        ReserveOutcome::TookOverFromSameCoordinator { stale_launch_id } => {
            warn!(
                "launch `{stale_launch_id}` still held this daemon's reservation when its \
                 coordinator `{}` reserved for launch `{}`; that launch is over and its \
                 release never landed, so the reservation moves to the new launch",
                decoded.coordinator_core_node, decoded.launch_id
            );
        }
        ReserveOutcome::AlreadyHeld => {}
    }

    ParticipantReserveResponse::accepted(&context.peppy_version, &context.root_instance_id)
        .encode()
        .map_err(Into::into)
}

/// Decodes every deployment's pins and refuses any that could not be
/// materialized here: a pin that arrived over the wire must carry a git
/// origin, because a filesystem origin names a tree on the machine that
/// minted it.
///
/// A path arriving from a coordinator is untrusted input. Not because the
/// coordinator is assumed hostile, but because a daemon that trusts a path
/// it was handed cannot distinguish a coordinator from anything else able
/// to reach it; the decode is where every structural rule fires, before any
/// filesystem or network is touched.
fn validate_deployment_pins(pins_json5: &[String]) -> std::result::Result<(), String> {
    for (index, raw) in pins_json5.iter().enumerate() {
        let pins: DeploymentPins = serde_json5::from_str(raw)
            .map_err(|e| format!("deployment pins #{index} are not decodable: {e}"))?;
        // The same rule the coordinator applied before it ever dispatched,
        // stated once: a misconfiguration must not read as two different
        // problems depending on which machine caught it.
        for pin in pins.items() {
            if let Some(reason) = crate::services::node::pins::portable_pin_refusal(pin) {
                return Err(reason);
            }
        }
    }
    Ok(())
}

/// Turns the reservation into a LEASE: while `lease` covers this daemon's
/// reservation for `coordinator`, it watches that core node's presence and
/// releases the moment the token disappears.
///
/// Without this, a coordinator that died mid-launch would wedge every machine
/// it had reserved until each daemon restarted, with nothing in the UI to
/// explain the refusals. That is the exact failure mode this plan refuses to
/// accept for cross-daemon pairing, so it must not be introduced one level up.
///
/// The task lives exactly as long as the reservation it supervises. Ending the
/// lease (a release, a `stack reset`, or this task's own) stops the watch, so a
/// daemon that serves launch after launch holds one presence subscription
/// rather than one per launch it has ever taken part in. The lease is also what
/// `release_because_coordinator_gone` is scoped to, so a watch that did outlive
/// its reservation frees nothing: not a later coordinator's reservation, and
/// not the next one taken by the coordinator it was watching.
fn watch_coordinator_presence(context: &FederationServiceContext, coordinator: &str, lease: Lease) {
    let messenger = context.messenger.clone();
    let coordinator = coordinator.to_owned();
    let ownership = Arc::clone(&context.ownership);
    let shutdown = context.shutdown_token.clone();

    tokio::spawn(async move {
        let watch = match CoreNodePresenceMessenger::watch(&messenger, &coordinator).await {
            Ok(watch) => watch,
            Err(e) => {
                // Losing the watch means losing the lease, which would leave a
                // reservation only a daemon restart could clear. Refuse to hold
                // one we cannot supervise.
                warn!(
                    "cannot watch coordinator `{coordinator}` presence ({e}); \
                     releasing its reservation rather than holding one this daemon \
                     cannot supervise"
                );
                ownership.release_because_coordinator_gone(&lease);
                return;
            }
        };

        loop {
            let event = tokio::select! {
                biased;
                _ = shutdown.cancelled() => return,
                // The reservation ended some other way, so there is nothing
                // left to supervise and the subscription can go.
                _ = lease.ended() => return,
                event = watch.rx.recv_async() => match event {
                    Ok(event) => event,
                    // The presence stream ended, so this reservation has lost
                    // its lease. Same verdict as failing to open the watch at
                    // all: release rather than hold one nothing supervises,
                    // which only a daemon restart could then clear.
                    Err(_) => {
                        warn!(
                            "presence stream for coordinator `{coordinator}` ended; releasing \
                             its reservation rather than holding one this daemon can no longer \
                             supervise"
                        );
                        ownership.release_because_coordinator_gone(&lease);
                        return;
                    }
                },
            };

            let LivelinessEvent::Gone(_) = event else {
                continue;
            };
            // Stop watching as soon as the reservation is no longer ours to
            // supervise, whether we released it here or it had already gone.
            match ownership.release_because_coordinator_gone(&lease) {
                Some(launch_id) => {
                    warn!(
                        "coordinator `{coordinator}` left the federation; released its \
                         reservation for launch `{launch_id}`. This machine's slice of that \
                         launch is still running: clear it with `peppy stack reset`."
                    );
                    return;
                }
                None => return,
            }
        }
    });
}

/// The commit point of a federated launch on this machine: the coordinator has
/// every participant reserved, so this daemon's slice is now replaced and the
/// host paths its containers will bind are prepared.
///
/// Destructive, and gated on the reservation. A request naming a launch this
/// daemon is not reserved for is refused, which is what stops a stale
/// coordinator, or one whose lease already lapsed, from wiping a machine out
/// from under the launch that legitimately owns it.
///
/// The bind sources are prepared HERE, in the window between clearing the slice
/// and running the first node of the new one, because that is the only moment
/// this machine has no container running: registering a host path the container
/// VM has not seen restarts it, and a restart takes every container in it. The
/// coordinator resolved the paths, since a mount path can name an instance
/// parameter and this daemon is handed one instance at a time.
async fn slice_begin_inner(
    request: &ServiceRequestContext,
    context: &FederationServiceContext,
) -> Result<Payload> {
    let decoded = ParticipantSliceBeginRequest::decode(request.message().payload_bytes().as_ref())?;

    let Some((held_launch, coordinator)) = context.ownership.held_reservation() else {
        return ParticipantSliceBeginResponse::refused(format!(
            "this daemon holds no reservation, so it will not replace its stack for launch \
             `{}`. The reservation is a lease on the coordinator's presence; if the coordinator \
             dropped off the federation it was released, and the launch has to start over.",
            decoded.launch_id
        ))
        .encode()
        .map_err(Into::into);
    };
    if held_launch != decoded.launch_id {
        return ParticipantSliceBeginResponse::refused(format!(
            "this daemon is reserved for launch `{held_launch}` driven by core node \
             `{coordinator}`, not `{}`",
            decoded.launch_id
        ))
        .encode()
        .map_err(Into::into);
    }

    debug!(
        "Replacing this daemon's stack slice for launch `{}` driven by `{coordinator}`",
        decoded.launch_id
    );

    crate::services::stack::clear_stack_slice(
        &context.messenger,
        &context.core_node_name,
        &context.root_instance_id,
        &context.node_stack,
        context.relationships.observation(),
    )
    .await;

    // Record the slice BEFORE the coordinator dispatches a single node to it.
    // The slice is what makes this machine's participation discoverable, and a
    // launch that dies halfway must still be findable by `stack reset
    // --federated`: recording only on success would leave exactly the wreckage
    // that needs cleaning up as the one state nobody can find.
    context
        .ownership
        .record_slice(LaunchIdentity::new(decoded.launch_id.clone(), coordinator));

    // Recorded first, prepared second: the slice is already this launch's, so a
    // machine that cannot provide a bind source is a refusal the coordinator
    // acts on, not wreckage nobody can find.
    match crate::services::stack::prepare_container_mounts(&decoded.mount_sources).await {
        Ok(auto_created) => ParticipantSliceBeginResponse::ok(auto_created),
        Err(reason) => ParticipantSliceBeginResponse::refused(format!(
            "this daemon cannot prepare the container bind sources for launch `{}`: {reason}",
            decoded.launch_id
        )),
    }
    .encode()
    .map_err(Into::into)
}

/// Records this daemon's half of a cross-daemon pair, on behalf of the daemon
/// that started the other endpoint.
///
/// A refusal here makes the requester revert its own half, so a pair is never
/// left established on one machine and absent on the other.
async fn pair_commit_inner(
    request: &ServiceRequestContext,
    context: &FederationServiceContext,
) -> Result<Payload> {
    let decoded = PairCommitRequest::decode(request.message().payload_bytes().as_ref())?;

    debug!(
        "Received `pair_commit` for `{}`:`{}` with `{}` on `{}`",
        decoded.local.instance_id,
        decoded.local_link_id,
        decoded.peer.instance_id,
        decoded.peer.core_node
    );

    let response = match context
        .relationships
        .pairing()
        .commit_pair_from_peer(&decoded)
        .await
    {
        Ok(()) => FederationVerdict::ok(),
        Err(reason) => FederationVerdict::refused(reason),
    };
    response.encode().map_err(Into::into)
}

async fn release_inner(
    request: &ServiceRequestContext,
    context: &FederationServiceContext,
) -> Result<Payload> {
    let decoded = ParticipantReleaseRequest::decode(request.message().payload_bytes().as_ref())?;

    let response = if context.ownership.release(&decoded.launch_id) {
        debug!("Released reservation for launch `{}`", decoded.launch_id);
        FederationVerdict::ok()
    } else {
        let held = context
            .ownership
            .held_reservation()
            .map(|(launch_id, _)| launch_id)
            .unwrap_or_default();
        FederationVerdict::refused(format!(
            "this daemon is reserved for launch `{held}`, not `{}`",
            decoded.launch_id
        ))
    };

    response.encode().map_err(Into::into)
}

/// Applies a peer daemon's report about an instance it owns.
///
/// Everything here is idempotent, because the notification is best-effort: the
/// sending daemon stays authoritative and only reports what already happened,
/// so a duplicate changes nothing and a lost one leaves this daemon stale
/// rather than in disagreement with its peer.
async fn notify_inner(
    request: &ServiceRequestContext,
    context: &FederationServiceContext,
) -> Result<Payload> {
    let decoded = RelationshipNotification::decode(request.message().payload_bytes().as_ref())?;

    debug!(
        "Received `relationship_notify` for `{}` on `{}`: {:?}",
        decoded.instance.instance_id, decoded.instance.core_node, decoded.event
    );

    match decoded.event {
        // A fresh incarnation of a remote source, reported with the number
        // its owner allocated. Feeds the same fan-out a local source's
        // lifecycle event feeds, so an observer drops and redeclares its
        // subscription onto the new run's keyexpr across a remote restart
        // exactly as it does across a local one.
        RelationshipEvent::ReachedRunning { incarnation } => {
            context
                .relationships
                .observation()
                .remote_source_reached_running(
                    &decoded.instance.core_node,
                    &decoded.instance.instance_id,
                    incarnation,
                )
                .await;
        }
        // Death-driven dissolution, the one relationship event that genuinely
        // crosses daemons at runtime. Authoritative on the daemon that owns the
        // dead instance; this side only converges.
        RelationshipEvent::Stopped => {
            context
                .relationships
                .observation()
                .remote_source_stopped(&decoded.instance.core_node, &decoded.instance.instance_id)
                .await;
            context
                .relationships
                .pairing()
                .dissolve_pairs_with_remote_instance(
                    &decoded.instance.core_node,
                    &decoded.instance.instance_id,
                )
                .await;
        }
    }

    RelationshipNotificationAck::new()
        .encode()
        .map_err(Into::into)
}

/// Answers another daemon's incarnation query for instances this daemon
/// owns. Authoritative: this daemon allocated every number it reports, and a
/// never-spawned instance reports zero, which the asker treats as "never
/// ran" exactly as its own ledger would.
async fn incarnation_query_inner(
    request: &ServiceRequestContext,
    context: &FederationServiceContext,
) -> Result<Payload> {
    let decoded = IncarnationQueryRequest::decode(request.message().payload_bytes().as_ref())?;
    let ledger = context.relationships.incarnations();
    let entries = decoded
        .instance_ids
        .iter()
        .map(|instance_id| IncarnationEntry {
            instance_id: instance_id.clone(),
            incarnation: ledger.current(&crate::services::node::incarnation::SourceKey::new(
                context.core_node_name.as_str(),
                instance_id.as_str(),
            )),
        })
        .collect();
    IncarnationQueryResponse { entries }
        .encode()
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pins_json5(origin: &str) -> String {
        format!(
            r#"{{ root: {{ kind: "node", name: "camera", tag: "v1", sha256: "{}",
                      origin: {origin} }} }}"#,
            "c".repeat(64)
        )
    }

    /// Decoding is the validation: a structurally valid, git-pinned set is
    /// accepted without touching this machine's filesystem or caches.
    #[test]
    fn a_git_pinned_deployment_validates() {
        let raw = pins_json5(&format!(
            r#"{{ source_type: "git", repo_url: "https://example.com/hub",
                  commit: "{}", path: "camera/peppy.json5" }}"#,
            "d".repeat(40)
        ));
        assert!(validate_deployment_pins(&[raw]).is_ok());
    }

    /// A pin that arrived over the wire carrying a filesystem origin names a
    /// tree only its minting machine could read, so it is refused while the
    /// reservation is still non-destructive.
    #[test]
    fn a_filesystem_pinned_deployment_is_refused() {
        let raw = pins_json5(r#"{ source_type: "fs", path: "/repo/camera/peppy.json5" }"#);
        let err = validate_deployment_pins(&[raw]).expect_err("an fs pin cannot cross machines");
        assert!(err.contains("coordinator's filesystem"), "{err}");
        assert!(err.contains("git repository"), "{err}");
    }

    /// The structural rules fire at decode: a traversal path is refused
    /// before any filesystem or network is touched, whoever sent it.
    #[test]
    fn a_pin_that_does_not_decode_is_refused() {
        let raw = pins_json5(&format!(
            r#"{{ source_type: "git", repo_url: "https://example.com/hub",
                  commit: "{}", path: "../../etc/passwd" }}"#,
            "d".repeat(40)
        ));
        let err = validate_deployment_pins(&[raw]).expect_err("a traversal path must be refused");
        assert!(err.contains("not decodable"), "{err}");
    }
}
