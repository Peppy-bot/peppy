//! Shared core for the framework slot-update services (`peer_update`,
//! `observation_update`). Both deliver ABSOLUTE per-slot state, keyed by the
//! node's own slot link_id, and share one protocol: registered pre-setup so
//! delivery never waits on user `setup_fn`; daemon-authoritative, so a caller
//! whose core_node is not this node's own daemon is rejected before slot state
//! is touched; and sequence-gated and idempotent, so a delayed retry can never
//! roll a slot back (a strictly-smaller sequence is stale, an equal one
//! asserts the state the slot already holds, a larger one supersedes).
//!
//! Each service supplies only its request type via [`SlotUpdate`]: the wire
//! decode, the slot key, and the whole slot state one absolute request
//! asserts. The daemon-only guard, the sequence gate, unknown-slot rejection,
//! and the shared [`SlotUpdateResponse`] ack live here once.

use crate::encoding::slot_update::SlotUpdateResponse;
use crate::messaging::{SenderTarget, ServiceRequestContext};
use crate::runtime::TaskHandle;
use crate::types::Payload;
use crate::{MessengerHandle, PeppyResult, ServiceMessenger};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::watch;
use tracing::{debug, warn};

/// One absolute slot-update request. The type carries the wire fields; this
/// trait supplies everything the shared service core needs to route and apply
/// it. Implemented by the per-service request types (`PeerUpdateRequest`,
/// `ObservationUpdateRequest`).
pub(crate) trait SlotUpdate: Sized {
    /// The per-slot watch payload this update replaces. Compared whole, so a
    /// repeat of a sequence is checked against what the slot already holds.
    type State: Clone + PartialEq + Send + Sync + 'static;

    /// Wire service name, also used in the daemon-only rejection message.
    const SERVICE: &'static str;
    /// Human noun for the unknown-slot rejection: "pairing slot" / "observer
    /// slot".
    const UNKNOWN_SLOT_NOUN: &'static str;

    fn decode_request(payload: &[u8]) -> PeppyResult<Self>;
    fn link_id(&self) -> &str;
    fn sequence(&self) -> u64;

    /// The sequence the slot currently holds, read for the stale-delivery gate.
    fn state_sequence(state: &Self::State) -> u64;

    /// The whole slot state this update asserts. A delivery carries every
    /// member the slot holds, so the shared core compares and replaces rather
    /// than merging member by member.
    fn to_state(&self) -> Self::State;

    /// Extra structured fields for the receipt debug log, beyond
    /// link_id/sequence (e.g. `paired=true`).
    fn log_detail(&self) -> String;
}

/// Shared map of one watch channel per declared slot, keyed by the node's own
/// slot link_id. Built once by the `Processor`; the map itself is immutable
/// (slots are declared in the manifest), only the channel values move.
pub(crate) type SlotSenders<S> = Arc<BTreeMap<String, watch::Sender<S>>>;

/// Registers the slot-update service `U::SERVICE` and drives its request loop.
/// Each service's public `listen_for_*` is a one-line call to this.
pub(crate) async fn listen_for_slot_update<U>(
    messenger: &MessengerHandle,
    core_node: &str,
    instance_id: &str,
    as_identity: SenderTarget,
    slots: SlotSenders<U::State>,
) -> PeppyResult<TaskHandle<PeppyResult<()>>>
where
    U: SlotUpdate + 'static,
{
    let mut endpoint =
        ServiceMessenger::listen(messenger, core_node, instance_id, as_identity, U::SERVICE)
            .await?;

    let daemon_core_node = core_node.to_string();
    let handle = crate::runtime::spawn(async move {
        endpoint
            .handle_requests(|context| {
                let slots = Arc::clone(&slots);
                let daemon_core_node = daemon_core_node.clone();
                async move { handle_slot_update_request::<U>(context, &daemon_core_node, &slots) }
            })
            .await
    });
    Ok(handle)
}

fn handle_slot_update_request<U>(
    context: ServiceRequestContext,
    daemon_core_node: &str,
    slots: &BTreeMap<String, watch::Sender<U::State>>,
) -> PeppyResult<Payload>
where
    U: SlotUpdate,
{
    let caller_core_node = context.message().core_node();
    if caller_core_node != daemon_core_node {
        warn!(
            service = U::SERVICE,
            caller_core_node = %caller_core_node,
            caller_instance_id = %context.message().instance_id(),
            "slot update from a caller outside this node's daemon; rejecting"
        );
        return SlotUpdateResponse::rejected(format!(
            "{} is daemon-only: caller core_node '{caller_core_node}' is not this node's daemon \
             '{daemon_core_node}'",
            U::SERVICE
        ))
        .encode();
    }
    let request = U::decode_request(&context.message().payload_bytes())?;
    debug!(
        service = U::SERVICE,
        link_id = %request.link_id(),
        sequence = request.sequence(),
        detail = %request.log_detail(),
        "received slot update from {}",
        context.message().instance_id(),
    );
    apply_slot_update::<U>(slots, &request).encode()
}

/// Applies one absolute-state update to the slot's watch channel. Split from the
/// service handler so tests can drive it without a wire round-trip.
pub(crate) fn apply_slot_update<U>(
    slots: &BTreeMap<String, watch::Sender<U::State>>,
    request: &U,
) -> SlotUpdateResponse
where
    U: SlotUpdate,
{
    let Some(sender) = slots.get(request.link_id()) else {
        warn!(
            service = U::SERVICE,
            link_id = %request.link_id(),
            "slot update names an undeclared slot; rejecting"
        );
        return SlotUpdateResponse::rejected(format!(
            "unknown {} '{}'",
            U::UNKNOWN_SLOT_NOUN,
            request.link_id()
        ));
    };
    let asserted = request.to_state();
    let mut stale = false;
    let mut conflicting = false;
    sender.send_if_modified(|state| {
        let held = U::state_sequence(state);
        if request.sequence() < held {
            stale = true;
            return false;
        }
        if request.sequence() == held {
            // One sequence, one state: a repeat of the sequence the slot
            // already holds is an idempotent retry and asserts the same
            // members. Two different states under one sequence are two answers
            // to one question, and applying either leaves the slot disagreeing
            // with whoever sent the other.
            conflicting = *state != asserted;
            return false;
        }
        *state = asserted.clone();
        true
    });
    if stale {
        return SlotUpdateResponse::stale();
    }
    if conflicting {
        return SlotUpdateResponse::rejected(format!(
            "{} for {} `{}` repeats sequence {} asserting different state",
            U::SERVICE,
            U::UNKNOWN_SLOT_NOUN,
            request.link_id(),
            request.sequence()
        ));
    }
    SlotUpdateResponse::accepted()
}
