//! Cap'n Proto codec for the framework `observation_update` service
//! (observer-slot delivery). See `schemas/observation_update.capnp` for the wire
//! contract.

use crate::error::{Error, Result};
use crate::messaging::{ObservedMemberState, ObservedSource, PeerInfo, ProducerRef};
use crate::observation_update_capnp;
use crate::types::Payload;

/// Absolute observer-slot state pushed by the daemon: the slot's complete
/// ordered member set. Field-for-field mirror of the capnp
/// `ObservationUpdateRequest`, with each `ObservedMember` decoding into the
/// [`ObservedMemberState`] the slot's watch channel holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationUpdateRequest {
    pub link_id: String,
    pub sequence: u64,
    pub members: Vec<ObservedMemberState>,
}

impl ObservationUpdateRequest {
    pub fn encode(&self) -> Result<Payload> {
        let mut builder = ::capnp::message::Builder::new_default();
        {
            let mut root = builder
                .init_root::<observation_update_capnp::observation_update_request::Builder>();
            root.set_link_id(&self.link_id);
            root.set_sequence(self.sequence);
            let mut members = root.init_members(self.members.len() as u32);
            for (idx, member) in self.members.iter().enumerate() {
                let mut wire = members.reborrow().get(idx as u32);
                wire.set_source_core_node(&member.source.producer.core_node);
                wire.set_source_instance_id(&member.source.producer.instance_id);
                wire.set_source_link_id(&member.source.source_link_id);
                wire.set_source_generation(member.source_generation);
                wire.set_source_live(member.source_live);
                if let Some(peer) = &member.source.peer {
                    let mut wire_peer = wire.init_peer();
                    wire_peer.set_core_node(&peer.producer.core_node);
                    wire_peer.set_instance_id(&peer.producer.instance_id);
                    wire_peer.set_link_id(&peer.peer_link_id);
                }
            }
        }
        super::encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = super::decode_message(data)?;
        let root = reader
            .get_root::<observation_update_capnp::observation_update_request::Reader>()
            .map_err(|e| Error::Deserialization(e.to_string()))?;
        let link_id = super::read_text(root.get_link_id(), "observation_update", "linkId")?;
        let sequence = root.get_sequence();
        let wire_members = root
            .get_members()
            .map_err(|e| Error::Deserialization(e.to_string()))?;
        let mut members: Vec<ObservedMemberState> = Vec::with_capacity(wire_members.len() as usize);
        for idx in 0..wire_members.len() {
            let wire = wire_members.get(idx);
            let peer = if wire.has_peer() {
                let wire_peer = wire
                    .get_peer()
                    .map_err(|e| Error::Deserialization(e.to_string()))?;
                Some(PeerInfo {
                    producer: ProducerRef::new(
                        super::read_text(
                            wire_peer.get_core_node(),
                            "observation_update",
                            "peer.coreNode",
                        )?,
                        super::read_text(
                            wire_peer.get_instance_id(),
                            "observation_update",
                            "peer.instanceId",
                        )?,
                    ),
                    peer_link_id: super::read_text(
                        wire_peer.get_link_id(),
                        "observation_update",
                        "peer.linkId",
                    )?,
                })
            } else {
                None
            };
            let source = ObservedSource {
                producer: ProducerRef::new(
                    super::read_text(
                        wire.get_source_core_node(),
                        "observation_update",
                        "sourceCoreNode",
                    )?,
                    super::read_text(
                        wire.get_source_instance_id(),
                        "observation_update",
                        "sourceInstanceId",
                    )?,
                ),
                source_link_id: super::read_text(
                    wire.get_source_link_id(),
                    "observation_update",
                    "sourceLinkId",
                )?,
                peer,
            };
            // A member's identity is its `(source, source_link_id, peer)`
            // triple, and a slot holds each identity once. Repeating one would
            // give the slot two subscriptions to one pairing and make the
            // position of every later member ambiguous, so the delivery is
            // refused whole. A slot's member set is small, so the
            // already-decoded members are the lookup: no side table, and no
            // per-member clone to key it.
            if members.iter().any(|member| member.source == source) {
                return Err(Error::Deserialization(format!(
                    "observation_update for slot `{link_id}` lists `{}/{}` on `{}` twice: a \
                     slot observes each pairing once",
                    source.producer.instance_id, source.source_link_id, source.producer.core_node,
                )));
            }
            members.push(ObservedMemberState {
                source,
                source_generation: wire.get_source_generation(),
                source_live: wire.get_source_live(),
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

    fn member(instance: &str, generation: u64, live: bool) -> ObservedMemberState {
        ObservedMemberState {
            source: ObservedSource {
                producer: ProducerRef::new("core_a", instance),
                source_link_id: "commander".to_string(),
                peer: None,
            },
            source_generation: generation,
            source_live: live,
        }
    }

    /// A member pinned to one pair of `instance`'s slot, by the pair's other
    /// end.
    fn pinned_member(instance: &str, peer_instance: &str) -> ObservedMemberState {
        ObservedMemberState {
            source: ObservedSource {
                producer: ProducerRef::new("core_a", instance),
                source_link_id: "commander".to_string(),
                peer: Some(PeerInfo {
                    producer: ProducerRef::new("core_b", peer_instance),
                    peer_link_id: "arm".to_string(),
                }),
            },
            source_generation: 1,
            source_live: true,
        }
    }

    fn round_trip(request: &ObservationUpdateRequest) -> ObservationUpdateRequest {
        ObservationUpdateRequest::decode(&request.encode().unwrap().into_inner()).unwrap()
    }

    /// Zero, one and several members all ride the same shape; the empty set is
    /// both the boot state and a `zero_or_more` slot the plan left unobserved.
    #[test]
    fn request_round_trips_every_member_count() {
        let empty = ObservationUpdateRequest {
            link_id: "observed_arm".to_string(),
            sequence: 43,
            members: Vec::new(),
        };
        assert_eq!(round_trip(&empty), empty);

        let sole = ObservationUpdateRequest {
            link_id: "observed_arm".to_string(),
            sequence: 42,
            members: vec![member("arm_1", 7, true)],
        };
        assert_eq!(round_trip(&sole), sole);

        let several = ObservationUpdateRequest {
            link_id: "observed_arm".to_string(),
            sequence: 44,
            members: vec![
                member("arm_2", 1, true),
                member("arm_1", 9, false),
                ObservedMemberState {
                    source: ObservedSource {
                        producer: ProducerRef::new("core_b", "arm_3"),
                        source_link_id: "gripper".to_string(),
                        peer: None,
                    },
                    source_generation: 3,
                    source_live: true,
                },
                pinned_member("engine_1", "ctrl_2"),
            ],
        };
        let decoded = round_trip(&several);
        assert_eq!(decoded, several);
        assert_eq!(
            decoded
                .members
                .iter()
                .map(|m| m.source.producer.instance_id.as_str())
                .collect::<Vec<_>>(),
            ["arm_2", "arm_1", "arm_3", "engine_1"],
            "plan order survives the wire"
        );
    }

    /// One source instance observed through two of ITS own pairing slots is two
    /// distinct members, as are two of its pairs on one slot observed each by
    /// its peer, while the same pairing listed twice is refused.
    #[test]
    fn decode_rejects_a_repeated_member_identity() {
        let repeated = ObservationUpdateRequest {
            link_id: "observed_arm".to_string(),
            sequence: 1,
            members: vec![member("arm_1", 1, true), member("arm_1", 2, true)],
        };
        let error = ObservationUpdateRequest::decode(&repeated.encode().unwrap().into_inner())
            .expect_err("a repeated member identity must be rejected");
        assert!(
            error.to_string().contains("observed_arm") && error.to_string().contains("arm_1"),
            "got: {error}"
        );

        let two_slots_of_one_source = ObservationUpdateRequest {
            link_id: "observed_arm".to_string(),
            sequence: 1,
            members: vec![
                member("arm_1", 1, true),
                ObservedMemberState {
                    source: ObservedSource {
                        producer: ProducerRef::new("core_a", "arm_1"),
                        source_link_id: "gripper".to_string(),
                        peer: None,
                    },
                    source_generation: 1,
                    source_live: true,
                },
            ],
        };
        assert_eq!(
            round_trip(&two_slots_of_one_source),
            two_slots_of_one_source
        );

        let two_pairs_of_one_slot = ObservationUpdateRequest {
            link_id: "observed_arm".to_string(),
            sequence: 1,
            members: vec![
                pinned_member("engine_1", "ctrl_1"),
                pinned_member("engine_1", "ctrl_2"),
            ],
        };
        assert_eq!(round_trip(&two_pairs_of_one_slot), two_pairs_of_one_slot);
    }
}
