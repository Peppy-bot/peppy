//! Cap'n Proto codec for the framework `binding_update` service
//! (producer-binding delivery). See `schemas/binding_update.capnp` for the wire
//! contract.

use crate::binding_update_capnp;
use crate::error::{Error, Result};
use crate::types::Payload;
use config::runtime::{BoundProducers, ProducerRef};

/// Absolute producer-binding state pushed by the daemon: the consumer slot's
/// complete ordered producer set. Field-for-field mirror of the capnp
/// `BindingUpdateRequest`.
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
            let producers = self.producers.as_slice();
            let mut wire = root.init_producers(producers.len() as u32);
            for (idx, producer) in producers.iter().enumerate() {
                let mut entry = wire.reborrow().get(idx as u32);
                entry.set_core_node(&producer.core_node);
                entry.set_instance_id(&producer.instance_id);
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
        let producers = wire
            .iter()
            .map(|entry| {
                Ok(ProducerRef::new(
                    super::read_text(entry.get_core_node(), "binding_update", "coreNode")?,
                    super::read_text(entry.get_instance_id(), "binding_update", "instanceId")?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        // A slot binds each producer once, so a repeated one refuses the
        // delivery whole, the rule the boot config's parse holds too.
        let producers = BoundProducers::try_from(producers).map_err(|error| {
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

    fn request(producers: Vec<ProducerRef>) -> BindingUpdateRequest {
        BindingUpdateRequest {
            link_id: "cameras".to_string(),
            sequence: 7,
            producers: BoundProducers::try_from(producers).expect("distinct producers"),
        }
    }

    #[test]
    fn a_producer_set_round_trips_in_plan_order() {
        let sent = request(vec![
            ProducerRef::new("core_a", "rear"),
            ProducerRef::new("core_b", "front"),
        ]);
        let received = BindingUpdateRequest::decode(&sent.encode().unwrap().into_inner()).unwrap();
        assert_eq!(received, sent);
    }

    #[test]
    fn an_empty_set_round_trips() {
        let sent = request(Vec::new());
        let received = BindingUpdateRequest::decode(&sent.encode().unwrap().into_inner()).unwrap();
        assert_eq!(received, sent);
    }

    #[test]
    fn a_repeated_producer_refuses_the_delivery() {
        let mut builder = ::capnp::message::Builder::new_default();
        {
            let mut root =
                builder.init_root::<binding_update_capnp::binding_update_request::Builder>();
            root.set_link_id("cameras");
            root.set_sequence(1);
            let mut wire = root.init_producers(2);
            for idx in 0..2 {
                let mut entry = wire.reborrow().get(idx);
                entry.set_core_node("core_a");
                entry.set_instance_id("front");
            }
        }
        let payload = super::super::encode_message(&builder).unwrap();
        let error = BindingUpdateRequest::decode(&payload.into_inner()).unwrap_err();
        assert!(error.to_string().contains("cameras"), "{error}");
    }
}
