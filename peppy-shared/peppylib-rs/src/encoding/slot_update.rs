//! Shared Cap'n Proto codec for the framework slot-update ack response, taken
//! by both the `peer_update` and `observation_update` services. See
//! `schemas/slot_update.capnp` for the wire contract.

use crate::error::{Error, Result};
use crate::slot_update_capnp;
use crate::types::Payload;

/// Node-side reply to a slot-update request (pairing or observation).
/// `accepted = false` with `stale_sequence = true` means the request's sequence
/// was strictly older than the slot's current one (a delayed retry) — the daemon
/// treats that as already-superseded, not as a failure to revert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotUpdateResponse {
    pub accepted: bool,
    pub stale_sequence: bool,
    pub message: String,
}

impl SlotUpdateResponse {
    pub fn accepted() -> Self {
        Self {
            accepted: true,
            stale_sequence: false,
            message: String::new(),
        }
    }

    pub fn stale() -> Self {
        Self {
            accepted: false,
            stale_sequence: true,
            message: "stale sequence".to_string(),
        }
    }

    pub fn rejected(message: impl Into<String>) -> Self {
        Self {
            accepted: false,
            stale_sequence: false,
            message: message.into(),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = ::capnp::message::Builder::new_default();
        {
            let mut root = builder.init_root::<slot_update_capnp::slot_update_response::Builder>();
            root.set_accepted(self.accepted);
            root.set_stale_sequence(self.stale_sequence);
            root.set_message(&self.message);
        }
        super::encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = super::decode_message(data)?;
        let root = reader
            .get_root::<slot_update_capnp::slot_update_response::Reader>()
            .map_err(|e| Error::Deserialization(e.to_string()))?;
        Ok(Self {
            accepted: root.get_accepted(),
            stale_sequence: root.get_stale_sequence(),
            message: super::read_text(root.get_message(), "slot_update", "message")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_round_trips_all_shapes() {
        for response in [
            SlotUpdateResponse::accepted(),
            SlotUpdateResponse::stale(),
            SlotUpdateResponse::rejected("unknown pairing slot 'arm'"),
        ] {
            let decoded =
                SlotUpdateResponse::decode(&response.encode().unwrap().into_inner()).unwrap();
            assert_eq!(decoded, response);
        }
    }
}
