//! Shared Cap'n Proto codec for the framework slot-update ack response, taken
//! by both the `peer_update` and `observation_update` services. See
//! `schemas/slot_update.capnp` for the wire contract.

use crate::error::{Error, Result};
use crate::slot_update_capnp;
use crate::types::Payload;

/// Node-side reply to a slot-update request (pairing, observation or
/// binding) that the node did not refuse. `stale_sequence = true` means the
/// request's sequence was strictly older than the slot's current one (a
/// delayed retry): the daemon treats that as already superseded, not as a
/// failure to revert. A refused delivery is an error reply with the failure
/// kind `rejected`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotUpdateResponse {
    pub stale_sequence: bool,
}

impl SlotUpdateResponse {
    /// The slot holds the delivered state from now on.
    pub fn taken() -> Self {
        Self {
            stale_sequence: false,
        }
    }

    /// The slot already holds a later delivery and kept it.
    pub fn stale() -> Self {
        Self {
            stale_sequence: true,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = ::capnp::message::Builder::new_default();
        {
            let mut root = builder.init_root::<slot_update_capnp::slot_update_response::Builder>();
            root.set_stale_sequence(self.stale_sequence);
        }
        super::encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = super::decode_message(data)?;
        let root = reader
            .get_root::<slot_update_capnp::slot_update_response::Reader>()
            .map_err(|e| Error::Deserialization(e.to_string()))?;
        Ok(Self {
            stale_sequence: root.get_stale_sequence(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_round_trips_all_shapes() {
        for response in [SlotUpdateResponse::taken(), SlotUpdateResponse::stale()] {
            let decoded =
                SlotUpdateResponse::decode(&response.encode().unwrap().into_inner()).unwrap();
            assert_eq!(decoded, response);
        }
    }
}
