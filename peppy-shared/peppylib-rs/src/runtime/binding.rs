//! Consumer-side runtime for producer-binding slots: [`BoundSetSubscription`]
//! receives one topic from every producer currently bound to a consumer slot.
//!
//! A consumer slot's producer set is the boot config's binding until the daemon
//! delivers another over `binding_update`. The subscription follows it through
//! the shared [`crate::runtime::slot_stream`] engine, one wire subscription per
//! bound producer, pinned to the producer's `(core_node, instance_id)`: a
//! producer joining the set is subscribed, and one leaving it is dropped along
//! with anything it had buffered.

use crate::error::{Error, Result};
use crate::messaging::{
    BoundSetState, MessengerHandle, ProducerRef, SenderTarget, Subscription, TopicMessenger,
};
use crate::runtime::slot_stream::{
    FollowedSlot, SlotStream, StreamWiring, published_by_producer, start_slot_stream,
};
use crate::runtime::{CancellationToken, NodeRunner};
use crate::types::Message;
use config::node::QoSProfile;
use tokio::sync::watch;

/// The consumer slot kind for the shared slot_stream engine: one pin per bound
/// producer, keyed on its wire address.
struct BoundFollow;

impl FollowedSlot for BoundFollow {
    type State = BoundSetState;
    type Pin = ProducerRef;
    type Wire = SenderTarget;

    fn desired(state: &BoundSetState) -> Vec<ProducerRef> {
        state.producers.producers().cloned().collect()
    }

    fn is_followed(state: &BoundSetState, pin: &ProducerRef) -> bool {
        state.producers.contains(pin)
    }

    fn producer(pin: &ProducerRef) -> &ProducerRef {
        pin
    }

    async fn subscribe(wiring: &StreamWiring<Self>, pin: &ProducerRef) -> Result<Subscription> {
        TopicMessenger::subscribe(
            &wiring.messenger,
            &wiring.as_core_node,
            &wiring.as_instance_id,
            wiring.wire.clone(),
            &wiring.topic,
            pin,
            wiring.qos.clone(),
        )
        .await
    }

    fn published_by(pin: &ProducerRef, message: &Message) -> bool {
        published_by_producer(pin, message)
    }
}

/// Stream of one topic from every producer bound to a consumer slot, merged
/// client-side. Message order is preserved independently per producer, with no
/// total ordering across producers; ready producers are merged fairly, so a busy
/// one cannot starve a quiet one. Delivery is a live stream, never a mailbox.
pub struct BoundSetSubscription {
    stream: SlotStream<BoundFollow>,
    shutdown: CancellationToken,
}

impl BoundSetSubscription {
    /// The next message from any producer currently bound to the slot, tagged
    /// with the producer that published it. Returns `None` once the node is
    /// shutting down and no queued message remains, or when the runtime is torn
    /// down. An empty set yields nothing until then. A message buffered from a
    /// producer the slot has since dropped never surfaces.
    pub async fn on_next_message(&mut self) -> Option<(ProducerRef, Message)> {
        tokio::select! {
            biased;
            next = self.stream.next() => {
                next.map(|(producer, message)| ((*producer).clone(), message))
            }
            _ = self.shutdown.cancelled() => None,
        }
    }
}

/// Subscribe to one topic across a consumer slot's bound producer set, for
/// every cardinality. Spliced by the generated consumed-topic `subscribe()`
/// call sites; `from_target` is the node or contract target the producers serve
/// the topic under. Declares a wire subscription for every producer bound now
/// and fails if one cannot be declared; a declaration failing while the stream
/// runs is declared again on a backoff.
pub async fn subscribe_bound_set(
    node_runner: &NodeRunner,
    link_id: &str,
    from_target: SenderTarget,
    topic: &str,
    qos: QoSProfile,
) -> Result<BoundSetSubscription> {
    let processor = node_runner.processor();
    let watch_rx =
        processor
            .bound_set_watch(link_id)
            .ok_or_else(|| Error::UnknownProducerSlot {
                link_id: link_id.to_string(),
            })?;
    subscribe_bound_set_with_watch(
        node_runner.messenger().clone(),
        processor.bound_core_node().to_string(),
        processor.bound_instance_id().to_string(),
        watch_rx,
        from_target,
        topic.to_string(),
        qos,
        node_runner.cancellation_token().clone(),
    )
    .await
}

/// Messenger-level core of [`subscribe_bound_set`]: the same engine driven by
/// an explicit watch channel. Nodes call [`subscribe_bound_set`]; this seam
/// serves embedders and tests that manage binding state themselves. `shutdown`
/// bounds the wait on an empty set and ends the stream at node stop, and the
/// stream also ends when `watch_rx`'s sender drops.
#[allow(clippy::too_many_arguments)]
pub async fn subscribe_bound_set_with_watch(
    messenger: MessengerHandle,
    as_core_node: String,
    as_instance_id: String,
    watch_rx: watch::Receiver<BoundSetState>,
    from_target: SenderTarget,
    topic: String,
    qos: QoSProfile,
    shutdown: CancellationToken,
) -> Result<BoundSetSubscription> {
    let wiring = StreamWiring {
        messenger,
        as_core_node,
        as_instance_id,
        wire: from_target,
        topic,
        qos,
    };
    Ok(BoundSetSubscription {
        stream: start_slot_stream::<BoundFollow>(wiring, watch_rx).await?,
        shutdown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::slot_stream::FollowedSlot;
    use config::runtime::BoundProducers;

    /// `is_followed` answers as `desired` does, member by member, so the
    /// engine's read-time filter and its converge task agree on the set.
    #[test]
    fn is_followed_agrees_with_desired() {
        let front = ProducerRef::new("core_a", "front");
        let rear = ProducerRef::new("core_a", "rear");
        let state = BoundSetState::seeded(
            BoundProducers::try_from(vec![front.clone(), rear.clone()]).unwrap(),
        );
        let desired = BoundFollow::desired(&state);
        assert_eq!(desired, [front.clone(), rear.clone()]);
        for pin in &desired {
            assert!(BoundFollow::is_followed(&state, pin));
        }
        assert!(!BoundFollow::is_followed(
            &state,
            &ProducerRef::new("core_a", "side")
        ));

        let shrunk = BoundSetState::seeded(BoundProducers::try_from(vec![rear.clone()]).unwrap());
        assert!(!BoundFollow::is_followed(&shrunk, &front));
        assert_eq!(BoundFollow::desired(&shrunk), [rear]);
    }
}
