//! `clock_offset` framework service.
//!
//! Every node exposes this. On request, the node performs an NTP-style exchange
//! against the core node (via [`crate::clock::synchronize`]) and replies with its
//! measured offset and round-trip delay. `peppy stack benchmark`, running inside
//! the core daemon, polls it per producer to normalize cross-host topic
//! timestamps into the core-node clock base.
//!
//! Like `node_health`, this is framework plumbing — it never invokes any user
//! interface handler, so it does not violate the benchmark's no-trigger
//! guarantee.

use std::sync::Arc;

use core_node_api::encoding::{ClockOffsetRequest, ClockOffsetResponse};
use tracing::debug;

use crate::messaging::{CLOCK_OFFSET_SERVICE, SenderTarget, ServiceRequestContext};
use crate::runtime::{NodeRunner, TaskHandle};
use crate::types::Payload;
use crate::{PeppyError, PeppyResult, ServiceMessenger};

pub async fn listen_for_clock_offset(
    node_runner: Arc<NodeRunner>,
    as_identity: SenderTarget,
) -> PeppyResult<TaskHandle<PeppyResult<()>>> {
    // Scope the processor borrow so it ends before `node_runner` is moved into
    // the spawned task below.
    let mut endpoint = {
        let processor = node_runner.processor();
        ServiceMessenger::listen(
            node_runner.messenger(),
            processor.bound_core_node(),
            processor.bound_instance_id(),
            as_identity,
            CLOCK_OFFSET_SERVICE,
        )
        .await?
    };

    let handle = crate::runtime::spawn(async move {
        endpoint
            .handle_requests(move |context| {
                let node_runner = Arc::clone(&node_runner);
                async move { handle_clock_offset_request(&node_runner, context).await }
            })
            .await
    });
    Ok(handle)
}

async fn handle_clock_offset_request(
    node_runner: &NodeRunner,
    context: ServiceRequestContext,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id().to_string();

    // Validate the (empty) request structurally so any wire-schema skew surfaces
    // loudly instead of being silently ignored.
    ClockOffsetRequest::decode(context.message().payload_bytes().as_ref()).map_err(|err| {
        PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.clone(),
            reason: format!("invalid clock_offset request: {err}"),
        }
    })?;
    debug!("Received `clock_offset` request from {sender_instance_id}");

    let sync = crate::clock::synchronize(node_runner, None)
        .await
        .map_err(|err| PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id,
            reason: format!("clock synchronize against core node failed: {err}"),
        })?;

    ClockOffsetResponse::new(sync.offset_ns, sync.round_trip_delay_ns)
        .encode()
        .map_err(Into::into)
}
