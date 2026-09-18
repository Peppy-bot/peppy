//! Cap'n Proto codec for the framework `peer_update` service (pairing-slot
//! delivery). See `schemas/peer_update.capnp` for the wire contract.

use crate::error::{Error, Result};
use crate::messaging::{PeerInfo, PeerMember, ProducerRef};
use crate::peer_update_capnp;
use crate::types::Payload;

/// Absolute pairing-slot state pushed by the daemon: every pair the slot
/// holds, in establishment order. Field-for-field mirror of the capnp
/// `PeerUpdateRequest`, with each `PeerMember` decoding into the
/// [`PeerMember`] the slot's watch channel holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerUpdateRequest {
    pub link_id: String,
    pub sequence: u64,
    pub members: Vec<PeerMember>,
}

impl PeerUpdateRequest {
    pub fn encode(&self) -> Result<Payload> {
        let mut builder = ::capnp::message::Builder::new_default();
        {
            let mut root = builder.init_root::<peer_update_capnp::peer_update_request::Builder>();
            root.set_link_id(&self.link_id);
            root.set_sequence(self.sequence);
            let mut members = root.init_members(self.members.len() as u32);
            for (idx, member) in self.members.iter().enumerate() {
                let mut wire = members.reborrow().get(idx as u32);
                wire.set_peer_core_node(&member.info.producer.core_node);
                wire.set_peer_instance_id(&member.info.producer.instance_id);
                wire.set_peer_link_id(&member.info.peer_link_id);
                wire.set_copy(member.copy.as_deref().unwrap_or(""));
            }
        }
        super::encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = super::decode_message(data)?;
        let root = reader
            .get_root::<peer_update_capnp::peer_update_request::Reader>()
            .map_err(|e| Error::Deserialization(e.to_string()))?;
        let link_id = super::read_text(root.get_link_id(), "peer_update", "linkId")?;
        let sequence = root.get_sequence();
        let wire_members = root
            .get_members()
            .map_err(|e| Error::Deserialization(e.to_string()))?;
        let mut members: Vec<PeerMember> = Vec::with_capacity(wire_members.len() as usize);
        for idx in 0..wire_members.len() {
            let wire = wire_members.get(idx);
            let info = PeerInfo {
                producer: ProducerRef::new(
                    super::read_text(wire.get_peer_core_node(), "peer_update", "peerCoreNode")?,
                    super::read_text(wire.get_peer_instance_id(), "peer_update", "peerInstanceId")?,
                ),
                peer_link_id: super::read_text(
                    wire.get_peer_link_id(),
                    "peer_update",
                    "peerLinkId",
                )?,
            };
            // A pair is one peer to this slot, and a slot holds each peer
            // once, so a delivery naming one twice is refused whole. A slot's
            // set is small, so the already-decoded members are the lookup.
            if members.iter().any(|member| member.info == info) {
                return Err(Error::Deserialization(format!(
                    "peer_update for slot `{link_id}` names peer `{}/{}` (slot `{}`) twice",
                    info.producer.core_node, info.producer.instance_id, info.peer_link_id
                )));
            }
            let copy = super::read_text(wire.get_copy(), "peer_update", "copy")?;
            members.push(PeerMember {
                info,
                copy: (!copy.is_empty()).then_some(copy),
            });
        }
        Ok(Self {
            link_id,
            sequence,
            members,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(instance: &str, copy: Option<&str>) -> PeerMember {
        PeerMember {
            info: PeerInfo {
                producer: ProducerRef::new("core_a", instance),
                peer_link_id: "controller".to_string(),
            },
            copy: copy.map(str::to_string),
        }
    }

    #[test]
    fn request_round_trips_every_set_size() {
        for members in [
            Vec::new(),
            vec![member("arm_1", None)],
            vec![
                member("alpha_arm", Some("alpha")),
                member("bravo_arm", Some("bravo")),
            ],
        ] {
            let request = PeerUpdateRequest {
                link_id: "arm".to_string(),
                sequence: 42,
                members,
            };
            let decoded =
                PeerUpdateRequest::decode(&request.encode().unwrap().into_inner()).unwrap();
            assert_eq!(decoded, request);
        }
    }

    #[test]
    fn a_peer_named_twice_is_refused_whole() {
        let request = PeerUpdateRequest {
            link_id: "arm".to_string(),
            sequence: 1,
            members: vec![member("arm_1", None), member("arm_1", Some("alpha"))],
        };
        let err = PeerUpdateRequest::decode(&request.encode().unwrap().into_inner())
            .expect_err("a repeated peer is refused");
        assert!(err.to_string().contains("twice"), "{err}");
    }
}
