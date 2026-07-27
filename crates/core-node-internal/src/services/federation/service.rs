//! The three daemon-to-daemon endpoints of a federated launch: reserve,
//! release, and the runtime relationship notification.
//!
//! The reservation handler is where the lease is wired: accepting a
//! reservation also spawns a watch on the coordinator's core-node presence, so
//! [`SliceOwnership::release_because_coordinator_gone`] fires the moment the
//! coordinator leaves the federation. The state machine itself lives in the
//! parent module and is tested there without any of this plumbing.

use super::{ReserveOutcome, SliceOwnership};
use crate::Result;
use crate::services::node::{RelationshipCoordinators, manifest_fingerprint, resolve_node_config};
use crate::services::response::into_service_response;
use core_node_api::ServiceId;
use core_node_api::encoding::{
    ParticipantReleaseRequest, ParticipantReleaseResponse, ParticipantReserveRequest,
    ParticipantReserveResponse, RelationshipEvent, RelationshipNotification,
    RelationshipNotificationAck, ResolvedManifest,
};
use daemon_config::launcher::DeploymentSource;
use core_node_api::names;
use daemon_config::consts::PeppyDirs;
use peppylib::messaging::{SenderTarget, ServiceRequestContext};
use peppylib::types::Payload;
use peppylib::{
    CoreNodePresenceMessenger, LivelinessEvent, MessengerHandle, PeppyResult, ServiceMessenger,
};
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

pub(crate) async fn listen_for_participant_reserve(
    context: FederationServiceContext,
    node_name: &str,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        &context.messenger,
        &context.core_node_name,
        &context.root_instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        ServiceId::ParticipantReserve.name(),
    )
    .await?;

    Ok(tokio::spawn(async move {
        endpoint
            .handle_requests(|request| handle_reserve(request, context.clone()))
            .await
            .map_err(Into::into)
    }))
}

async fn handle_reserve(
    request: ServiceRequestContext,
    context: FederationServiceContext,
) -> PeppyResult<Payload> {
    into_service_response(&request, reserve_inner(&request, &context).await)
}

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
async fn resolve_slice_manifests(
    context: &FederationServiceContext,
    sources_json5: &[String],
) -> std::result::Result<Vec<ResolvedManifest>, String> {
    let mut manifests = Vec::with_capacity(sources_json5.len());
    for (index, source_json5) in sources_json5.iter().enumerate() {
        let source: DeploymentSource = serde_json5::from_str(source_json5)
            .map_err(|e| format!("deployment source #{index} is not decodable: {e}"))?;
        let source = crate::services::stack::off_coordinator_node_source(&source, &context.peppy_dirs)?;
        let config = resolve_node_config(source, &context.peppy_dirs)
            .await
            .map_err(|e| format!("deployment source #{index} failed to resolve: {e}"))?;
        let fingerprint = manifest_fingerprint(&config)?;
        let config_json5 = json5_pretty::to_string_pretty(&config)
            .map_err(|e| format!("deployment source #{index} failed to serialize: {e}"))?;
        manifests.push(ResolvedManifest::new(config_json5, fingerprint));
    }
    Ok(manifests)
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

pub(crate) async fn listen_for_participant_release(
    context: FederationServiceContext,
    node_name: &str,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        &context.messenger,
        &context.core_node_name,
        &context.root_instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        ServiceId::ParticipantRelease.name(),
    )
    .await?;

    Ok(tokio::spawn(async move {
        endpoint
            .handle_requests(|request| handle_release(request, context.clone()))
            .await
            .map_err(Into::into)
    }))
}

async fn handle_release(
    request: ServiceRequestContext,
    context: FederationServiceContext,
) -> PeppyResult<Payload> {
    into_service_response(&request, release_inner(&request, &context))
}

fn release_inner(
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

pub(crate) async fn listen_for_relationship_notify(
    context: FederationServiceContext,
    node_name: &str,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        &context.messenger,
        &context.core_node_name,
        &context.root_instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        ServiceId::RelationshipNotify.name(),
    )
    .await?;

    Ok(tokio::spawn(async move {
        endpoint
            .handle_requests(|request| handle_notify(request, context.clone()))
            .await
            .map_err(Into::into)
    }))
}

async fn handle_notify(
    request: ServiceRequestContext,
    context: FederationServiceContext,
) -> PeppyResult<Payload> {
    into_service_response(&request, notify_inner(&request, &context).await)
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
                .remote_source_stopped(&decoded.core_node, &decoded.instance_id);
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
