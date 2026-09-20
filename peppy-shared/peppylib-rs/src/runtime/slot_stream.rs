//! Shared consumer-side forwarding engine for pinned slots (pairing peers and
//! observer sources). Both follow producers that the daemon delivers live over
//! a slot-update service, and both want the same wire-subscription lifecycle:
//!
//! - a pin the slot no longer follows (unpaired, or a member the plan dropped)
//!   → no wire subscription at all (nothing to receive, and no wildcard shape
//!   exists for a pinned consumer);
//! - a followed pin → exactly one wire subscription, triple-pinned to the
//!   producer's `(core_node, instance_id, producer-side link_id)`;
//! - a pin changes (re-pair, or a source-incarnation change) → the old
//!   subscription is dropped BEFORE the new one is declared (at most one wire
//!   subscription per followed pin, ever), and a delivery-time stale filter
//!   drops any already-buffered message tagged with a superseded pin.
//!
//! Each slot kind differs only in what it follows. A pairing slot follows its
//! one peer pin, so its set is empty while unpaired and holds one member once
//! paired. An observer slot follows one pin per member of its set, keyed on
//! `(source generation, source pin)` so a reused instance_id under an identical
//! wire triple is still told apart. [`FollowedSlot`] captures that difference;
//! the set convergence, the one-subscription-per-pin invariant, the fair merge
//! across members, the stale filter, and teardown live here once.

use crate::error::Result;
use crate::messaging::{MessengerHandle, ProducerRef, SenderTarget, Subscription, TopicMessenger};
use crate::runtime::TaskHandle;
use crate::types::Message;
use config::node::QoSProfile;
use pmi::PairingRecipient;
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use tracing::warn;

/// The kind of slot a [`SlotStream`] follows. An impl projects the slot's watch
/// state to the pins currently to follow (empty when the slot follows nothing)
/// and exposes each pin's wire coordinates.
pub(crate) trait FollowedSlot: Send + Sync + 'static {
    /// The per-slot watch payload delivered by the slot-update service.
    type State: Send + Sync + 'static;
    /// One followed pin. Its `PartialEq` is the load-bearing key: a wire
    /// subscription is (re)declared whenever a pin appears or changes, and a
    /// buffered message is dropped at delivery once its pin is no longer in the
    /// followed set.
    type Pin: Clone + PartialEq + Send + Sync + 'static;

    /// The pins to follow now, in the slot's own order, without duplicates.
    /// Empty when the slot follows nothing. Called only when the slot's state
    /// actually changed, so it is free to allocate.
    fn desired(state: &Self::State) -> Vec<Self::Pin>;
    /// Whether `pin` is still in the followed set. The same answer as
    /// `desired(state).contains(pin)`, without materializing the set: this one
    /// runs on the per-message delivery path.
    fn is_followed(state: &Self::State, pin: &Self::Pin) -> bool;
    /// The producer whose publishes this pin subscribes to.
    fn producer(pin: &Self::Pin) -> &ProducerRef;
    /// The producer-side link_id segment of that producer's publishes.
    fn producer_link_id(pin: &Self::Pin) -> &str;
    /// The recipient this pin's subscription stands for: the slot-wide
    /// `slot_recipient` for a participant pin and for an observation of a
    /// whole slot, or the one peer an observation is pinned to.
    fn recipient(_pin: &Self::Pin, slot_recipient: &PairingRecipient) -> Result<PairingRecipient> {
        Ok(slot_recipient.clone())
    }
}

/// One slot's live message stream. Owns the forwarding task (aborted on drop)
/// and applies the delivery-time staleness filter shared by both slot kinds.
pub(crate) struct SlotStream<S: FollowedSlot> {
    rx: mpsc::Receiver<(Arc<S::Pin>, Message)>,
    watch_rx: watch::Receiver<S::State>,
    forward_task: TaskHandle<()>,
}

impl<S: FollowedSlot> SlotStream<S> {
    /// The next message from any currently followed pin, as `(pin, message)`;
    /// each slot kind's subscription wrapper projects the pin into its
    /// user-facing identity type. `None` when the runtime is torn down (slot
    /// channel closed).
    ///
    /// A message tagged with a pin the slot has since moved off is dropped here.
    /// The wire triple pin makes a foreign producer unmatchable at the keyexpr
    /// level, but a pin swap (re-pair, a source-incarnation change under a
    /// reused triple, or a member leaving the set) can leave a message buffered
    /// under the old pin; this re-check against the live set drops it. The pin
    /// alone cannot always discriminate incarnations, so the slot kind folds any
    /// generation into `Pin`'s identity.
    pub(crate) async fn next(&mut self) -> Option<(Arc<S::Pin>, Message)> {
        loop {
            let (pin, message) = self.rx.recv().await?;
            let still_followed = S::is_followed(&self.watch_rx.borrow(), &pin);
            if still_followed {
                return Some((pin, message));
            }
            // Stale: buffered under a pin the slot has since moved off.
        }
    }
}

/// Aborting the forwarding task on drop (rather than relying on its `tx.send`
/// erroring) matters because the task only touches `tx` when a message arrives:
/// an inactive or quiet slot leaves it parked on `watch_rx.changed()`, where it
/// would outlive the dropped stream — wire subscription included — until the
/// next slot update.
impl<S: FollowedSlot> Drop for SlotStream<S> {
    fn drop(&mut self) {
        self.forward_task.abort();
    }
}

/// Spawns the forwarding task and returns the stream. The public
/// `subscribe_*_with_watch` seams are one-line calls to this.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_slot_stream<S: FollowedSlot>(
    messenger: MessengerHandle,
    as_core_node: String,
    as_instance_id: String,
    watch_rx: watch::Receiver<S::State>,
    pairing_target: SenderTarget,
    recipient: PairingRecipient,
    topic: String,
    qos: QoSProfile,
) -> SlotStream<S> {
    let (tx, rx) = mpsc::channel(super::SLOT_CHANNEL_CAPACITY);
    let forward_task = crate::runtime::spawn(forward_messages::<S>(
        messenger,
        as_core_node,
        as_instance_id,
        watch_rx.clone(),
        pairing_target,
        recipient,
        topic,
        qos,
        tx,
    ));
    SlotStream {
        rx,
        watch_rx,
        forward_task,
    }
}

/// The eager forwarding loop: keeps one wire subscription per followed pin
/// converged with the slot's set, tagging each forwarded message with the pin it
/// arrived under. Ends when the slot channel closes (runtime teardown) or the
/// stream is dropped (its `Drop` aborts this task).
#[allow(clippy::too_many_arguments)]
async fn forward_messages<S: FollowedSlot>(
    messenger: MessengerHandle,
    as_core_node: String,
    as_instance_id: String,
    mut watch_rx: watch::Receiver<S::State>,
    pairing_target: SenderTarget,
    recipient: PairingRecipient,
    topic: String,
    qos: QoSProfile,
    tx: mpsc::Sender<(Arc<S::Pin>, Message)>,
) {
    // One entry per followed pin, in the slot's own order. Each pin sits behind
    // an `Arc` so tagging a forwarded message is a refcount bump, not a pin
    // clone. At most one wire subscription per pin, ever.
    let mut current: Vec<(Arc<S::Pin>, Subscription)> = Vec::new();
    // Rotating first-poll position, so a busy member cannot indefinitely starve
    // a quiet one.
    let mut next_start: usize = 0;
    // Convergence is set work, so it runs only when the followed set can have
    // moved: the first pass, every slot update, and after a member is dropped.
    // The steady-state message path skips it entirely rather than rebuilding
    // and rediffing the whole pin set once per forwarded message.
    let mut needs_converge = true;
    loop {
        if needs_converge {
            let desired = S::desired(&watch_rx.borrow_and_update());
            current = converge_subscriptions::<S>(
                current,
                desired,
                &messenger,
                &as_core_node,
                &as_instance_id,
                &pairing_target,
                &recipient,
                &topic,
                &qos,
            )
            .await;
            needs_converge = false;
        }

        if current.is_empty() {
            if watch_rx.changed().await.is_err() {
                return; // runtime teardown
            }
            needs_converge = true;
            continue;
        }

        let start = next_start;
        next_start = next_start.wrapping_add(1);
        // `biased` polls the slot update before the members, so a pin swap is
        // applied promptly instead of waiting out a saturated stream; a slot
        // update is rare, so the members are reached on essentially every poll.
        let received = tokio::select! {
            biased;
            changed = watch_rx.changed() => {
                if changed.is_err() {
                    return; // runtime teardown
                }
                needs_converge = true;
                None
            }
            (idx, received) = crate::messaging::recv_first_ready(
                &current,
                |(_, subscription)| subscription.wire_receiver(),
                start,
            ) => Some((idx, received)),
        };

        match received {
            // The followed set moved; reconverge at the loop top.
            None => continue,
            Some((idx, Ok(raw))) => {
                let message = Message::from(raw);
                let (pin, _) = &current[idx];
                // The triple pin makes a foreign producer unmatchable at the
                // keyexpr level; this re-check is the defensive second guard.
                let producer = S::producer(pin);
                let matches_pin = message.core_node() == producer.core_node
                    && message.instance_id() == producer.instance_id
                    && message.link_id() == S::producer_link_id(pin);
                if matches_pin && tx.send((Arc::clone(pin), message)).await.is_err() {
                    return; // stream dropped
                }
            }
            Some((idx, Err(_))) => {
                // One member's wire channel closed (session teardown). Drop it
                // and keep serving the rest, reconverging at the very next loop
                // iteration rather than waiting for a slot update, so the pin is
                // redeclared right away if the slot still follows it.
                //
                // That immediate retry cannot spin: the channel closes only when
                // the messenger session dropped the subscriber's callback, and a
                // closing zenoh session takes its primitives out of the session
                // state BEFORE it drops any callback, so every declaration after
                // this point fails with `SessionClosedError`. The member is then
                // left out by `converge_subscriptions`, the set empties, and the
                // loop parks on the slot watch until teardown closes it.
                let (gone, _) = current.remove(idx);
                needs_converge = true;
                warn!(
                    topic = %topic,
                    core_node = %S::producer(&gone).core_node,
                    instance_id = %S::producer(&gone).instance_id,
                    "followed pin's wire subscription closed; continuing with the remaining pins"
                );
            }
        }
    }
}

/// Converges the live subscription set onto `desired`, preserving its order.
///
/// Drop-before-redeclare, per member: every pin the slot has moved off dies
/// here BEFORE any newly followed pin's subscription exists, so one pin never
/// holds two wire subscriptions across a change. A pin that is still followed
/// keeps the subscription it already had, so an unrelated member's change never
/// interrupts it. A member whose declaration fails is left out and retried at
/// the next slot update, which is also what leaves the whole slot silent when
/// nothing can be declared.
#[allow(clippy::too_many_arguments)]
async fn converge_subscriptions<S: FollowedSlot>(
    mut current: Vec<(Arc<S::Pin>, Subscription)>,
    desired: Vec<S::Pin>,
    messenger: &MessengerHandle,
    as_core_node: &str,
    as_instance_id: &str,
    pairing_target: &SenderTarget,
    slot_recipient: &PairingRecipient,
    topic: &str,
    qos: &QoSProfile,
) -> Vec<(Arc<S::Pin>, Subscription)> {
    current.retain(|(pin, _)| desired.contains(&**pin));

    // Claim the still-followed subscriptions first, so every pin the slot moved
    // off is already dropped before any new one is declared. One entry per
    // desired position, `None` where a declaration is still owed.
    let mut converged: Vec<Option<(Arc<S::Pin>, Subscription)>> = Vec::with_capacity(desired.len());
    let mut pending: Vec<(usize, S::Pin)> = Vec::new();
    for (position, pin) in desired.into_iter().enumerate() {
        match current.iter().position(|(followed, _)| **followed == pin) {
            Some(idx) => converged.push(Some(current.swap_remove(idx))),
            None => {
                converged.push(None);
                pending.push((position, pin));
            }
        }
    }

    // The owed declarations are mutually independent, so a multi-member slot
    // waits out one declare round-trip rather than N in series. Each result is
    // filed by its `desired` position, never by completion order.
    let declared = futures::future::join_all(pending.iter().map(|(_, pin)| async move {
        TopicMessenger::subscribe_peer_pinned(
            messenger,
            as_core_node,
            as_instance_id,
            pairing_target.clone(),
            S::producer(pin),
            S::producer_link_id(pin),
            S::recipient(pin, slot_recipient)?,
            topic,
            qos.clone(),
        )
        .await
    }))
    .await;

    for ((position, pin), result) in pending.into_iter().zip(declared) {
        match result {
            Ok(subscription) => converged[position] = Some((Arc::new(pin), subscription)),
            Err(err) => {
                warn!(
                    %err,
                    topic = %topic,
                    core_node = %S::producer(&pin).core_node,
                    instance_id = %S::producer(&pin).instance_id,
                    "failed to declare pinned wire subscription; this pin stays silent until the next slot update"
                );
            }
        }
    }
    // A pin whose declaration failed is left out and retried at the next slot
    // update; the remaining members keep their order.
    converged.into_iter().flatten().collect()
}
