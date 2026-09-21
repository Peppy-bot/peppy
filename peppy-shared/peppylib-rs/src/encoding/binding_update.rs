//! Cap'n Proto codec for the framework `binding_update` service
//! (producer-binding delivery). See `schemas/binding_update.capnp` for the wire
//! contract.

use crate::binding_update_capnp;
use crate::error::{Error, Result};
use crate::types::Payload;
use config::runtime::{BoundMember, BoundProducers, Name};

/// Absolute producer-binding state pushed by the daemon: the consumer slot's
/// complete ordered member set. Field-for-field mirror of the capnp
/// `BindingUpdateRequest`, with each wire `BoundMember` decoding into the
/// [`BoundMember`] the slot's watch channel holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingUpdateRequest {
    pub link_id: String,
    pub sequence: u64,
    pub producers: BoundProducers,
}

impl BindingUpdateRequest {
    pub fn encode(&self) -> Result<Payload> {
        let mut builder = ::capnp::message::Builder::new_default();
        {
            let mut root =
                builder.init_root::<binding_update_capnp::binding_update_request::Builder>();
            root.set_link_id(&self.link_id);
            root.set_sequence(self.sequence);
            let members = self.producers.as_slice();
            let mut wire = root.init_producers(members.len() as u32);
            for (idx, member) in members.iter().enumerate() {
                let mut entry = wire.reborrow().get(idx as u32);
                entry.set_core_node(&member.producer.core_node);
                entry.set_instance_id(&member.producer.instance_id);
                entry.set_copy(member.copy.as_ref().map(Name::as_str).unwrap_or(""));
            }
        }
        super::encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = super::decode_message(data)?;
        let root = reader
            .get_root::<binding_update_capnp::binding_update_request::Reader>()
            .map_err(|e| Error::Deserialization(e.to_string()))?;
        let link_id = super::read_text(root.get_link_id(), "binding_update", "linkId")?;
        let wire = root
            .get_producers()
            .map_err(|e| Error::Deserialization(e.to_string()))?;
        let members = wire
            .iter()
            .map(|entry| {
                let producer = super::read_producer(
                    entry.get_core_node(),
                    entry.get_instance_id(),
                    "binding_update",
                    ("coreNode", "instanceId"),
                )?;
                let copy = super::read_text(entry.get_copy(), "binding_update", "copy")?;
                let copy = (!copy.is_empty())
                    .then(|| Name::new(copy))
                    .transpose()
                    .map_err(|error| {
                        Error::Deserialization(format!(
                            "binding_update for slot `{link_id}`: member `{}/{}` names an \
                             invalid copy: {error}",
                            producer.core_node, producer.instance_id
                        ))
                    })?;
                Ok(BoundMember { producer, copy })
            })
            .collect::<Result<Vec<_>>>()?;
        // A slot binds each producer once, so a repeated one refuses the
        // delivery whole, the rule the boot config's parse holds too.
        let producers = BoundProducers::try_from(members).map_err(|error| {
            Error::Deserialization(format!("binding_update for slot `{link_id}`: {error}"))
        })?;
        Ok(Self {
            link_id,
            sequence: root.get_sequence(),
            producers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::runtime::ProducerRef;

    fn member(core_node: &str, instance_id: &str, copy: Option<&str>) -> BoundMember {
        BoundMember {
            producer: ProducerRef::new(core_node, instance_id),
            copy: copy.map(|copy| Name::new(copy).expect("a valid copy name")),
        }
    }

    fn request(members: Vec<BoundMember>) -> BindingUpdateRequest {
        BindingUpdateRequest {
            link_id: "cameras".to_string(),
            sequence: 7,
            producers: BoundProducers::try_from(members).expect("distinct producers"),
        }
    }

    #[test]
    fn a_member_set_round_trips_in_plan_order_with_each_copy() {
        for members in [
            Vec::new(),
            vec![member("core_a", "rear", None)],
            vec![
                member("core_a", "rear", None),
                member("core_b", "alpha_front", Some("alpha")),
                member("core_a", "bravo_front", Some("bravo")),
            ],
        ] {
            let sent = request(members);
            let received =
                BindingUpdateRequest::decode(&sent.encode().unwrap().into_inner()).unwrap();
            assert_eq!(received, sent);
        }
    }

    /// The wire refuses a producer named twice, whatever copies name it, and
    /// a copy that is not a valid name.
    #[test]
    fn a_repeated_producer_or_an_invalid_copy_refuses_the_delivery() {
        let payload = |copies: [&str; 2]| {
            let mut builder = ::capnp::message::Builder::new_default();
            {
                let mut root =
                    builder.init_root::<binding_update_capnp::binding_update_request::Builder>();
                root.set_link_id("cameras");
                root.set_sequence(1);
                let mut wire = root.init_producers(2);
                for (idx, copy) in copies.into_iter().enumerate() {
                    let mut entry = wire.reborrow().get(idx as u32);
                    entry.set_core_node("core_a");
                    entry.set_instance_id("front");
                    entry.set_copy(copy);
                }
            }
            super::super::encode_message(&builder).unwrap()
        };
        for (copies, expected) in [
            (["alpha", "bravo"], "Duplicate producer"),
            (["alpha", "not a name"], "invalid copy"),
        ] {
            let error = BindingUpdateRequest::decode(&payload(copies).into_inner()).unwrap_err();
            let message = error.to_string();
            assert!(
                message.contains("cameras") && message.contains(expected),
                "{message}"
            );
        }
    }
}
