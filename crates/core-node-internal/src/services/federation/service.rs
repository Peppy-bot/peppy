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

use super::{ReserveOutcome, SliceOwnership};
use crate::Result;
use crate::services::node::{
    RelationshipCoordinators, manifest_fingerprint_of_json5, resolve_node_config,
};
use crate::services::response::into_service_response;
use core_node_api::ServiceId;
use core_node_api::encoding::{
    LaunchIdentity, PairCommitRequest, PairCommitResponse, ParticipantReleaseRequest,
    ParticipantReleaseResponse, ParticipantReserveRequest, ParticipantReserveResponse,
    ParticipantSliceBeginRequest, ParticipantSliceBeginResponse, RelationshipEvent,
    RelationshipNotification, RelationshipNotificationAck, ResolvedManifest,
};
use core_node_api::names;
use daemon_config::consts::PeppyDirs;
use daemon_config::launcher::DeploymentSource;
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
    pub(crate) peppy_dirs: PeppyDirs,
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

async fn reserve_inner(
    request: &ServiceRequestContext,
    context: &FederationServiceContext,
) -> Result<Payload> {
    let decoded = ParticipantReserveRequest::decode(request.message().payload().as_ref())?;

    debug!(
        "Received `participant_reserve` for launch `{}` from coordinator `{}`",
        decoded.launch_id, decoded.coordinator_core_node
    );

    if let ReserveOutcome::HeldByAnotherLaunch {
        launch_id,
        coordinator_core_node,
    } = context
        .ownership
        .try_reserve(&decoded.launch_id, &decoded.coordinator_core_node)
    {
        // Refuse before resolving anything. The coordinator releases whatever
        // it did obtain and fails the launch, so no machine is left with a
        // half-replaced stack by a launch that never had a chance.
        return ParticipantReserveResponse::rejected(
            format!(
                "already reserved for launch `{launch_id}` driven by core node \
                 `{coordinator_core_node}`"
            ),
            &context.peppy_version,
        )
        .encode()
        .map_err(Into::into);
    }

    watch_coordinator_presence(context, &decoded.coordinator_core_node);

    // Resolve this slice's manifests here rather than accepting the
    // coordinator's. The coordinator then needs no reachability to sources it
    // does not itself use, and the manifest it validates is provably the one
    // this daemon will spawn from, because it is this daemon's own cache.
    let manifests = match resolve_slice_manifests(context, &decoded.deployment_sources_json5).await
    {
        Ok(manifests) => manifests,
        Err(reason) => {
            // A resolution failure is this participant's refusal, so drop the
            // reservation we just took rather than holding a machine hostage
            // to a launch that cannot proceed.
            context.ownership.release(&decoded.launch_id);
            return ParticipantReserveResponse::rejected(reason, &context.peppy_version)
                .encode()
                .map_err(Into::into);
        }
    };

    ParticipantReserveResponse::accepted(
        &context.peppy_version,
        &context.root_instance_id,
        manifests,
    )
    .encode()
    .map_err(Into::into)
}

/// Resolves one manifest per requested deployment source, in request order, so
/// the coordinator can align them with the deployments it asked about.
///
/// Concurrently: each source is a distinct git clone, download, or cache read
/// with nothing shared between them, and the whole call has to fit inside the
/// coordinator's preflight budget. Resolving them one at a time would spend
/// that budget on the sum of every fetch rather than the slowest one.
/// `try_join_all` preserves request order, which is what the alignment relies
/// on.
async fn resolve_slice_manifests(
    context: &FederationServiceContext,
    sources_json5: &[String],
) -> std::result::Result<Vec<ResolvedManifest>, String> {
    futures::future::try_join_all(sources_json5.iter().enumerate().map(
        |(index, source_json5)| async move {
            let source: DeploymentSource = serde_json5::from_str(source_json5)
                .map_err(|e| format!("deployment source #{index} is not decodable: {e}"))?;
            let source = crate::services::stack::portable_node_source(&source)?;
            let config = resolve_node_config(source, &context.peppy_dirs)
                .await
                .map_err(|e| format!("deployment source #{index} failed to resolve: {e}"))?;
            // Serialize once: the fingerprint is defined over exactly these
            // bytes, so hashing them is the same answer `manifest_fingerprint`
            // would reach after re-serializing.
            let config_json5 = json5_pretty::to_string_pretty(&config)
                .map_err(|e| format!("deployment source #{index} failed to serialize: {e}"))?;
            let fingerprint = manifest_fingerprint_of_json5(&config_json5);
            Ok(ResolvedManifest::new(config_json5, fingerprint))
        },
    ))
    .await
}

/// Turns the reservation into a LEASE: while this daemon holds a reservation
/// for `coordinator`, it watches that core node's presence and releases the
/// moment the token disappears.
///
/// Without this, a coordinator that died mid-launch would wedge every machine
/// it had reserved until each daemon restarted, with nothing in the UI to
/// explain the refusals. That is the exact failure mode this plan refuses to
/// accept for cross-daemon pairing, so it must not be introduced one level up.
///
/// The task is scoped to the coordinator it watches:
/// `release_because_coordinator_gone` is a no-op unless that same coordinator
/// still holds the reservation, so a watch outliving its reservation cannot
/// free a later one.
fn watch_coordinator_presence(context: &FederationServiceContext, coordinator: &str) {
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
                ownership.release_because_coordinator_gone(&coordinator);
                return;
            }
        };

        loop {
            let event = tokio::select! {
                biased;
                _ = shutdown.cancelled() => return,
                event = watch.rx.recv_async() => match event {
                    Ok(event) => event,
                    Err(_) => return,
                },
            };

            let LivelinessEvent::Gone(_) = event else {
                continue;
            };
            // Stop watching as soon as the reservation is no longer ours to
            // supervise, whether we released it here or it had already gone.
            match ownership.release_because_coordinator_gone(&coordinator) {
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
/// every participant reserved, so this daemon's slice is now replaced.
///
/// Destructive, and gated on the reservation. A request naming a launch this
/// daemon is not reserved for is refused, which is what stops a stale
/// coordinator, or one whose lease already lapsed, from wiping a machine out
/// from under the launch that legitimately owns it.
async fn slice_begin_inner(
    request: &ServiceRequestContext,
    context: &FederationServiceContext,
) -> Result<Payload> {
    let decoded = ParticipantSliceBeginRequest::decode(request.message().payload().as_ref())?;

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

    ParticipantSliceBeginResponse::began()
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
    let decoded = PairCommitRequest::decode(request.message().payload().as_ref())?;

    debug!(
        "Received `pair_commit` for `{}`:`{}` with `{}` on `{}`",
        decoded.local_instance_id,
        decoded.local_link_id,
        decoded.peer_instance_id,
        decoded.peer_core_node
    );

    let response = match context
        .relationships
        .pairing()
        .commit_pair_from_peer(&decoded)
        .await
    {
        Ok(()) => PairCommitResponse::committed(),
        Err(reason) => PairCommitResponse::refused(reason),
    };
    response.encode().map_err(Into::into)
}

async fn release_inner(
    request: &ServiceRequestContext,
    context: &FederationServiceContext,
) -> Result<Payload> {
    let decoded = ParticipantReleaseRequest::decode(request.message().payload().as_ref())?;

    let response = if context.ownership.release(&decoded.launch_id) {
        debug!("Released reservation for launch `{}`", decoded.launch_id);
        ParticipantReleaseResponse::released()
    } else {
        let held = context
            .ownership
            .held_reservation()
            .map(|(launch_id, _)| launch_id)
            .unwrap_or_default();
        ParticipantReleaseResponse::refused(format!(
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
    let decoded = RelationshipNotification::decode(request.message().payload().as_ref())?;

    debug!(
        "Received `relationship_notify` for `{}` on `{}`: {:?}",
        decoded.instance_id, decoded.core_node, decoded.event
    );

    match decoded.event {
        // A fresh incarnation of a remote source. Feeds the same fan-out a
        // local source's lifecycle event feeds, so an observer drops and
        // redeclares its subscription across a remote restart exactly as it
        // does across a local one.
        RelationshipEvent::ReachedRunning => {
            context
                .relationships
                .observation()
                .remote_source_reached_running(&decoded.core_node, &decoded.instance_id)
                .await;
        }
        // Death-driven dissolution, the one relationship event that genuinely
        // crosses daemons at runtime. Authoritative on the daemon that owns the
        // dead instance; this side only converges.
        RelationshipEvent::Stopped => {
            context
                .relationships
                .observation()
                .remote_source_stopped(&decoded.core_node, &decoded.instance_id)
                .await;
            context
                .relationships
                .pairing()
                .dissolve_pairs_with_remote_instance(&decoded.core_node, &decoded.instance_id)
                .await;
        }
    }

    RelationshipNotificationAck::received()
        .encode()
        .map_err(Into::into)
}
