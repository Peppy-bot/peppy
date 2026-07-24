//! Small glue helpers shared by the node command handlers that don't fit
//! in one of the more specific submodules: panic payload decoding,
//! random id generation, response encoding, and daemon-to-instance slot
//! update delivery.

use config::runtime::Name;
use node_stack::NodeStack;
use peppylib::MessengerHandle;
use peppylib::encoding::slot_update::SlotUpdateResponse;
use peppylib::messaging::{ProducerRef, SenderTarget, ServiceMessenger, ServiceTarget};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// A resolved daemon-to-instance service target. Observation fan-out resolves
/// this once per observer and reuses it for all of that observer's slots.
pub(crate) struct SlotUpdateTarget {
    sender: SenderTarget,
    producer: ProducerRef,
}

/// Shared transport and sequencing for the pairing and observation
/// coordinators' absolute-state update protocols.
pub(crate) struct SlotUpdateClient {
    node_stack: Arc<NodeStack>,
    messenger: MessengerHandle,
    core_node_name: String,
    caller_instance_id: String,
    seq: AtomicU64,
}

impl SlotUpdateClient {
    pub(crate) fn new(
        node_stack: Arc<NodeStack>,
        messenger: MessengerHandle,
        core_node_name: impl Into<String>,
        caller_instance_id: impl Into<String>,
    ) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(1);
        Self {
            node_stack,
            messenger,
            core_node_name: core_node_name.into(),
            caller_instance_id: caller_instance_id.into(),
            seq: AtomicU64::new(seed),
        }
    }

    pub(crate) fn node_stack(&self) -> &NodeStack {
        &self.node_stack
    }

    pub(crate) fn core_node_name(&self) -> &str {
        &self.core_node_name
    }

    pub(crate) fn next_sequence(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub(crate) fn resolve_target(
        &self,
        instance_id: &str,
    ) -> std::result::Result<SlotUpdateTarget, String> {
        let instance_name =
            Name::new(instance_id).map_err(|error| format!("invalid instance id: {error}"))?;
        let (node_name, node_tag) = self
            .node_stack
            .find_entity_label_for_instance_id_any_state(&instance_name)
            .ok_or_else(|| "instance is no longer tracked".to_string())?;
        Ok(SlotUpdateTarget {
            sender: SenderTarget::node(&node_name, &node_tag).map_err(|error| error.to_string())?,
            producer: ProducerRef::new(&self.core_node_name, instance_id),
        })
    }

    pub(crate) async fn send(
        &self,
        instance_id: &str,
        service_name: &str,
        payload: peppylib::types::Payload,
        timeout: Duration,
        rejection_message: &str,
    ) -> std::result::Result<(), String> {
        let target = self.resolve_target(instance_id)?;
        self.send_to(&target, service_name, payload, timeout, rejection_message)
            .await
    }

    pub(crate) async fn send_to(
        &self,
        target: &SlotUpdateTarget,
        service_name: &str,
        payload: peppylib::types::Payload,
        timeout: Duration,
        rejection_message: &str,
    ) -> std::result::Result<(), String> {
        let reply = ServiceMessenger::poll(
            &self.messenger,
            &self.core_node_name,
            &self.caller_instance_id,
            target.sender.clone(),
            service_name,
            ServiceTarget::Producer(&target.producer),
            payload,
            timeout,
        )
        .await
        .map_err(|error| error.to_string())?;

        let response = SlotUpdateResponse::decode(&reply.payload_bytes())
            .map_err(|error| error.to_string())?;
        if response.accepted || response.stale_sequence {
            Ok(())
        } else if response.message.is_empty() {
            Err(rejection_message.to_string())
        } else {
            Err(response.message)
        }
    }
}

/// Extract a human-readable message from a panic payload.
/// Used by spawned task handlers to convert panics into failure results.
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

/// Maps an encoding `Result` to a `PeppyResult`, wrapping the error as an
/// `InternalEncodingError` so it can be returned directly from a goal handler.
/// Used in place of open-coding the same `map_err` at every rejection and
/// accepted-response encoding site in the add/build/start handlers.
pub(crate) fn encode_response_or_err(
    identifier: &'static str,
    result: core_node_api::Result<peppylib::types::Payload>,
) -> peppylib::PeppyResult<peppylib::types::Payload> {
    result.map_err(|e| peppylib::PeppyError::InternalEncodingError {
        identifier: identifier.to_string(),
        reason: format!("Failed to encode response: {}", e),
    })
}

pub(crate) fn generate_random_id() -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    let bytes: [u8; 6] = rng.random();
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}
