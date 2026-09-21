//! Shared consumer-side engine for pinned slots (pairing peers, observer
//! sources and bound producers). Each follows producers that the daemon
//! delivers live over a slot-update service, and each wants the same
//! wire-subscription lifecycle:
//!
//! - a pin the slot no longer follows (unpaired, or a member the plan dropped)
//!   → no wire subscription at all (nothing to receive, and no wildcard shape
//!   exists for a pinned consumer);
//! - a followed pin → exactly one wire subscription, pinned to the producer's
//!   `(core_node, instance_id)` and, for a pairing or observer slot, its
//!   producer-side link_id;
//! - a pin changes (re-pair, or a source-incarnation change) → the old
//!   subscription is dropped BEFORE the new one is declared (at most one wire
//!   subscription per followed pin, ever), and a read-time stale filter drops
//!   any already-buffered message tagged with a superseded pin.
//!
//! Each slot kind differs only in what it follows. A pairing slot follows its
//! one peer pin, so its set is empty while unpaired and holds one member once
//! paired. An observer slot follows one pin per member of its set, keyed on
//! `(source generation, source pin)` so a reused instance_id under an identical
//! wire triple is still told apart. A consumer slot follows one pin per bound
//! producer. [`FollowedSlot`] captures what each kind follows and how it
//! subscribes one pin.
//!
//! A [`SlotStream`] splits the work in two. Its converge task owns the wire
//! subscriptions, keeps one per followed pin as the slot's set changes, and
//! publishes the followed members' wire receivers. The reader merges across
//! those receivers when it is asked for a message, polling them in a rotated
//! order, so every message waits in its own member's wire buffer until it is
//! read and a busy member never queues a quiet one behind its backlog.

use crate::error::Result;
use crate::messaging::{MessengerHandle, ProducerRef, Subscription};
use crate::runtime::TaskHandle;
use crate::types::Message;
use config::node::QoSProfile;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, watch};
use tracing::warn;

/// How long the converge task waits before declaring again a pin whose
/// declaration failed. Each further failure doubles the wait, up to
/// [`MAX_REDECLARE_DELAY`].
const FIRST_REDECLARE_DELAY: Duration = Duration::from_millis(500);

/// The longest wait between two declarations of a pin that keeps failing.
const MAX_REDECLARE_DELAY: Duration = Duration::from_secs(30);

/// The kind of slot a [`SlotStream`] follows. An impl projects the slot's watch
/// state to the pins currently to follow (empty when the slot follows nothing)
/// and declares the wire subscription each pin takes.
pub(crate) trait FollowedSlot: Sized + Send + Sync + 'static {
    /// The per-slot watch payload delivered by the slot-update service.
    type State: Send + Sync + 'static;
    /// One followed pin. Its `PartialEq` is the load-bearing key: a wire
    /// subscription is (re)declared whenever a pin appears or changes, and a
    /// buffered message is dropped at read time once its pin is no longer in
    /// the followed set.
    type Pin: Clone + PartialEq + Send + Sync + 'static;
    /// What every pin of one stream subscribes against, fixed for the stream's
    /// lifetime: the pairing's target and recipient for a pairing or observer
    /// slot, the node or contract target for a consumer slot.
    type Wire: Send + Sync + 'static;

    /// The pins to follow now, in the slot's own order, without duplicates.
    /// Empty when the slot follows nothing. Called only when the slot's state
    /// changed or a declaration is owed, so it is free to allocate.
    fn desired(state: &Self::State) -> Vec<Self::Pin>;
    /// Whether `pin` is still in the followed set. The same answer as
    /// `desired(state).contains(pin)`, without materializing the set: this one
    /// runs on the per-message read path.
    fn is_followed(state: &Self::State, pin: &Self::Pin) -> bool;
    /// The producer whose publishes this pin subscribes to.
    fn producer(pin: &Self::Pin) -> &ProducerRef;
    /// Declares the one wire subscription that follows `pin`, pinned to its
    /// producer.
    fn subscribe(
        wiring: &StreamWiring<Self>,
        pin: &Self::Pin,
    ) -> impl Future<Output = Result<Subscription>> + Send;
    /// Whether `message`, received on `pin`'s subscription, was published by
    /// the producer `pin` follows: the defensive second guard behind the
    /// pinned keyexpr.
    fn published_by(pin: &Self::Pin, message: &Message) -> bool;
}

/// What every declaration of one stream subscribes as and against, fixed for
/// the stream's lifetime.
pub(crate) struct StreamWiring<S: FollowedSlot> {
    pub(crate) messenger: MessengerHandle,
    pub(crate) as_core_node: String,
    pub(crate) as_instance_id: String,
    pub(crate) wire: S::Wire,
    pub(crate) topic: String,
    pub(crate) qos: QoSProfile,
}

/// One followed member as the reader polls it: its pin and a handle on the wire
/// receiver of the subscription the converge task holds for it.
type Member<S> = (
    Arc<<S as FollowedSlot>::Pin>,
    flume::Receiver<pmi::TopicMessage>,
);

/// The followed members in the slot's own order, as the converge task last
/// settled them.
type Members<S> = Arc<[Member<S>]>;

/// One slot's live message stream. Owns the converge task (aborted on drop)
/// and reads the followed members' wire buffers directly.
pub(crate) struct SlotStream<S: FollowedSlot> {
    /// The slot's own set, consulted on every read so a message buffered under
    /// a pin the slot has since moved off never surfaces.
    state_rx: watch::Receiver<S::State>,
    /// What the converge task publishes after every change it applies.
    members_rx: watch::Receiver<Members<S>>,
    /// The members this reader polls, reloaded from `members_rx` before the
    /// next read once the converge task has published a change.
    members: Members<S>,
    /// Rotating first-poll position, so a busy member cannot starve a quiet one.
    next_start: usize,
    /// Rung by the reader when a followed member's wire channel closes, so the
    /// converge task declares it again.
    closed: Arc<Notify>,
    converge_task: TaskHandle<()>,
}

/// Whether `message` carries the wire pair of `producer`: the two segments
/// every pinned subscription is keyed on.
pub(crate) fn published_by_producer(producer: &ProducerRef, message: &Message) -> bool {
    message.core_node() == producer.core_node && message.instance_id() == producer.instance_id
}

impl<S: FollowedSlot> SlotStream<S> {
    /// The next message from any currently followed pin, as `(pin, message)`;
    /// each slot kind's subscription wrapper projects the pin into its
    /// user-facing identity type. `None` once the runtime is torn down (the
    /// slot's state channel closed).
    ///
    /// A message is read from its member's own wire buffer. One tagged with a
    /// pin the slot has since moved off is dropped here: the pinned keyexpr
    /// makes a foreign producer unmatchable, but a pin swap (re-pair, a
    /// source-incarnation change under a reused wire address, or a member
    /// leaving the set) can leave a message buffered under the old pin. An
    /// observer slot folds the source's generation into its pin, which tells
    /// two incarnations under one wire address apart.
    ///
    /// A slot kind's wrapper may stop waiting sooner: the bound-set
    /// subscription also races the node's shutdown token, so a node on its way
    /// down stops awaiting a set that may never publish again.
    pub(crate) async fn next(&mut self) -> Option<(Arc<S::Pin>, Message)> {
        loop {
            if self.members.is_empty() {
                if self.members_rx.changed().await.is_err() {
                    return None; // runtime teardown
                }
                self.members = self.members_rx.borrow_and_update().clone();
                continue;
            }

            let start = self.next_start;
            self.next_start = self.next_start.wrapping_add(1);
            // `biased` prefers a published change over the members' buffers,
            // and the stale filter below is what makes a departed member's
            // buffer unreadable: the slot's state changes before the converge
            // task wakes, so `is_followed` already answers false for it.
            let received = tokio::select! {
                biased;
                changed = self.members_rx.changed() => {
                    if changed.is_err() {
                        return None; // runtime teardown
                    }
                    None
                }
                received = recv_first_ready(&self.members, start) => Some(received),
            };

            match received {
                None => self.members = self.members_rx.borrow_and_update().clone(),
                Some((idx, Ok(raw))) => {
                    let pin = &self.members[idx].0;
                    let message = Message::from(raw);
                    // One read guard on the slot's state: a second one taken
                    // while a delivery waits to write would never be granted.
                    let followed = S::is_followed(&self.state_rx.borrow(), pin);
                    if S::published_by(pin, &message) && followed {
                        return Some((Arc::clone(pin), message));
                    }
                }
                Some((idx, Err(_))) => {
                    // This member's wire channel closed: the converge task
                    // dropped its subscription because the slot moved off the
                    // pin, or the session closed it. The reader stops polling
                    // it, and the converge task's next publish carries the
                    // members it keeps. A pin the slot still follows is
                    // declared again: the converge task is rung, and counts a
                    // closed channel as a declaration it owes.
                    let pin = &self.members[idx].0;
                    if S::is_followed(&self.state_rx.borrow(), pin) {
                        let producer = S::producer(pin);
                        warn!(
                            core_node = %producer.core_node,
                            instance_id = %producer.instance_id,
                            "a followed member's wire channel closed; declaring it again"
                        );
                        self.closed.notify_one();
                    }
                    self.members = self
                        .members
                        .iter()
                        .enumerate()
                        .filter(|(position, _)| *position != idx)
                        .map(|(_, member)| member.clone())
                        .collect();
                }
            }
        }
    }
}

/// Dropping the stream aborts the converge task, which drops every wire
/// subscription with it. A quiet slot's task is parked on the next slot update,
/// so the abort is what ends it.
impl<S: FollowedSlot> Drop for SlotStream<S> {
    fn drop(&mut self) {
        self.converge_task.abort();
    }
}

/// Declares the wire subscription of every pin the slot follows now, then
/// spawns the converge task that keeps them in step with the slot. Fails with
/// the first declaration that fails, keeping none of them and logging each
/// failure: the caller is there to hear about it. A declaration failing after
/// the stream runs is retried by the converge task on a backoff.
pub(crate) async fn start_slot_stream<S: FollowedSlot>(
    wiring: StreamWiring<S>,
    mut state_rx: watch::Receiver<S::State>,
) -> Result<SlotStream<S>> {
    let desired = S::desired(&state_rx.borrow_and_update());
    let Converged { current, failed } =
        converge_subscriptions::<S>(Vec::new(), desired, &wiring).await;
    for (pin, error) in &failed {
        warn!(
            %error,
            topic = %wiring.topic,
            core_node = %S::producer(pin).core_node,
            instance_id = %S::producer(pin).instance_id,
            "failed to declare a pinned wire subscription at subscribe time"
        );
    }
    if let Some((_, error)) = failed.into_iter().next() {
        return Err(error);
    }
    let (members_tx, members_rx) = watch::channel(members_of::<S>(&current));
    let members = members_rx.borrow().clone();
    let closed = Arc::new(Notify::new());
    let converge_task = crate::runtime::spawn(follow_the_set::<S>(
        wiring,
        state_rx.clone(),
        current,
        members_tx,
        Arc::clone(&closed),
    ));
    Ok(SlotStream {
        state_rx,
        members_rx,
        members,
        next_start: 0,
        closed,
        converge_task,
    })
}

/// The converge loop: keeps one wire subscription per followed pin as the
/// slot's set changes, publishing the followed members after each change. A pin
/// whose declaration failed, or whose wire channel closed under it, is declared
/// again after a backoff, until it succeeds or the slot stops following it.
/// Ends when the slot's state channel closes (runtime teardown), dropping every
/// subscription it holds.
async fn follow_the_set<S: FollowedSlot>(
    wiring: StreamWiring<S>,
    mut state_rx: watch::Receiver<S::State>,
    mut current: Vec<(Arc<S::Pin>, Subscription)>,
    members_tx: watch::Sender<Members<S>>,
    closed: Arc<Notify>,
) {
    let mut redeclare_delay = FIRST_REDECLARE_DELAY;
    loop {
        let owed = owes_declarations::<S>(&state_rx.borrow(), &current);
        tokio::select! {
            changed = state_rx.changed() => {
                if changed.is_err() {
                    return; // runtime teardown
                }
            }
            () = closed.notified() => {}
            () = tokio::time::sleep(redeclare_delay), if owed => {
                redeclare_delay = (redeclare_delay * 2).min(MAX_REDECLARE_DELAY);
            }
        }

        let desired = S::desired(&state_rx.borrow_and_update());
        let Converged {
            current: converged,
            failed,
        } = converge_subscriptions::<S>(current, desired, &wiring).await;
        current = converged;
        for (pin, error) in &failed {
            warn!(
                %error,
                topic = %wiring.topic,
                core_node = %S::producer(pin).core_node,
                instance_id = %S::producer(pin).instance_id,
                "failed to declare a pinned wire subscription; declaring it again shortly"
            );
        }
        if failed.is_empty() {
            redeclare_delay = FIRST_REDECLARE_DELAY;
        }
        members_tx.send_if_modified(|held| {
            if follows_the_same_pins::<S>(held, &current) {
                return false;
            }
            *held = members_of::<S>(&current);
            true
        });
    }
}

/// Whether the slot follows a pin that holds no open wire subscription.
fn owes_declarations<S: FollowedSlot>(
    state: &S::State,
    current: &[(Arc<S::Pin>, Subscription)],
) -> bool {
    S::desired(state).iter().any(|pin| {
        !current
            .iter()
            .any(|(followed, subscription)| **followed == *pin && is_open(subscription))
    })
}

/// Whether a wire subscription's channel is still open: a closed one delivers
/// nothing more and is declared again.
fn is_open(subscription: &Subscription) -> bool {
    !subscription.wire_receiver().is_disconnected()
}

/// Whether the published members follow the same pins as `current`, in the
/// same order. A pin the converge task keeps keeps its subscription, so equal
/// pins mean equal receivers.
fn follows_the_same_pins<S: FollowedSlot>(
    held: &Members<S>,
    current: &[(Arc<S::Pin>, Subscription)],
) -> bool {
    held.len() == current.len()
        && held
            .iter()
            .zip(current.iter())
            .all(|((held_pin, _), (pin, _))| held_pin == pin)
}

/// The followed members as the reader polls them.
fn members_of<S: FollowedSlot>(current: &[(Arc<S::Pin>, Subscription)]) -> Members<S> {
    current
        .iter()
        .map(|(pin, subscription)| (Arc::clone(pin), subscription.wire_receiver().clone()))
        .collect()
}

/// What one convergence left: the subscriptions it holds, in the slot's order,
/// and each pin whose declaration failed.
struct Converged<S: FollowedSlot> {
    current: Vec<(Arc<S::Pin>, Subscription)>,
    failed: Vec<(S::Pin, crate::error::Error)>,
}

/// Converges the live subscription set onto `desired`, preserving its order.
///
/// Drop-before-redeclare, per member: every pin the slot has moved off dies
/// here BEFORE any newly followed pin's subscription exists, so one pin never
/// holds two wire subscriptions across a change. A pin that is still followed
/// keeps the subscription it already had, so an unrelated member's change never
/// interrupts it; one whose channel closed is dropped and declared again. A
/// member whose declaration fails is left out and reported.
async fn converge_subscriptions<S: FollowedSlot>(
    mut current: Vec<(Arc<S::Pin>, Subscription)>,
    desired: Vec<S::Pin>,
    wiring: &StreamWiring<S>,
) -> Converged<S> {
    current.retain(|(pin, subscription)| desired.contains(&**pin) && is_open(subscription));

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
    let declared =
        futures::future::join_all(pending.iter().map(|(_, pin)| S::subscribe(wiring, pin))).await;

    let mut failed = Vec::new();
    for ((position, pin), result) in pending.into_iter().zip(declared) {
        match result {
            Ok(subscription) => converged[position] = Some((Arc::new(pin), subscription)),
            Err(error) => failed.push((pin, error)),
        }
    }
    Converged {
        current: converged.into_iter().flatten().collect(),
        failed,
    }
}

/// First-ready-wins receive across the followed members, polled in a rotated
/// order starting at `start` so a busy member cannot indefinitely starve a
/// quiet one. Returns the winning index into `members` and its receive result;
/// the reader advances its own rotation cursor once per call.
///
/// This is the per-message read path of every pinned slot, so it boxes nothing:
/// one `Vec` of the members' receive futures, and flume's `RecvFut` is `Unpin`
/// (it goes into `select_all` as-is) and cancel-safe, so the losing futures drop
/// without consuming a message. The ubiquitous single-member set receives
/// directly, with no future collection at all.
async fn recv_first_ready<T>(
    members: &[(T, flume::Receiver<pmi::TopicMessage>)],
    start: usize,
) -> (
    usize,
    std::result::Result<pmi::TopicMessage, flume::RecvError>,
) {
    debug_assert!(
        !members.is_empty(),
        "`recv_first_ready` needs at least one member to poll"
    );
    let len = members.len();
    if len == 1 {
        return (0, members[0].1.recv_async().await);
    }
    let start = start % len;
    let recvs: Vec<_> = (0..len)
        .map(|offset| members[(start + offset) % len].1.recv_async())
        .collect();
    let (received, position, _) = futures::future::select_all(recvs).await;
    ((start + position) % len, received)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(instance_id: &str, body: String) -> pmi::TopicMessage {
        pmi::TopicMessage::from_parts(
            "core".to_string(),
            instance_id.to_string(),
            bytes::Bytes::from(body.into_bytes()),
        )
    }

    /// The fan-in rotates its first poll: with a busy member holding a backlog
    /// and a quiet member holding one message, the quiet message comes back
    /// within one full rotation, however long the backlog.
    #[tokio::test]
    async fn first_ready_receive_rotates_so_a_backlog_cannot_starve_a_quiet_member() {
        const BUSY_BACKLOG: usize = 50;
        let (busy_tx, busy_rx) = flume::unbounded();
        let (quiet_tx, quiet_rx) = flume::unbounded();
        for idx in 0..BUSY_BACKLOG {
            busy_tx
                .send(message("busy", format!("busy-{idx}")))
                .expect("the busy member is open");
        }
        quiet_tx
            .send(message("quiet", "quiet-0".to_string()))
            .expect("the quiet member is open");
        let members = [("busy", busy_rx), ("quiet", quiet_rx)];

        let mut quiet_served = false;
        for start in 0..members.len() {
            let (idx, received) = recv_first_ready(&members, start).await;
            received.expect("both members hold a message");
            if idx == 1 {
                quiet_served = true;
                break;
            }
        }
        assert!(
            quiet_served,
            "the quiet member must be served within one rotation of the first poll"
        );
    }

    /// A slot kind the test drives: the pins it follows come from a watch the
    /// test holds, and a declaration fails as many times as the test says
    /// before it is allowed to succeed.
    mod engine {
        use super::*;
        use crate::messaging::{MessengerHandle, ProducerRef, SenderTarget, TopicMessenger};
        use crate::types::Payload;
        use config::node::QoSProfile;
        use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter};
        use std::collections::HashMap;
        use std::sync::Mutex as StdMutex;
        use std::time::Duration;
        use tokio::sync::{Mutex, watch};

        const CORE: &str = "test_core";
        const TOPIC: &str = "joint_states";
        const READER: &str = "monitor_1";
        const PRODUCER_NODE: &str = "robot_arm";

        /// How many more times each producer's declaration fails, and how many
        /// declarations have been attempted.
        #[derive(Default)]
        struct Declarations {
            failures: StdMutex<HashMap<String, usize>>,
            attempts: StdMutex<usize>,
        }

        impl Declarations {
            fn failing(instance_id: &str, times: usize) -> Arc<Self> {
                let plan = Self::default();
                plan.failures
                    .lock()
                    .unwrap()
                    .insert(instance_id.to_string(), times);
                Arc::new(plan)
            }

            /// Whether this attempt at `instance_id` fails, spending one of its
            /// planned failures.
            fn fails(&self, instance_id: &str) -> bool {
                *self.attempts.lock().unwrap() += 1;
                let mut failures = self.failures.lock().unwrap();
                match failures.get_mut(instance_id) {
                    Some(left) if *left > 0 => {
                        *left -= 1;
                        true
                    }
                    _ => false,
                }
            }
        }

        struct TestWire {
            target: SenderTarget,
            declarations: Arc<Declarations>,
        }

        struct TestSlot;

        impl FollowedSlot for TestSlot {
            type State = Vec<ProducerRef>;
            type Pin = ProducerRef;
            type Wire = TestWire;

            fn desired(state: &Vec<ProducerRef>) -> Vec<ProducerRef> {
                state.clone()
            }

            fn is_followed(state: &Vec<ProducerRef>, pin: &ProducerRef) -> bool {
                state.contains(pin)
            }

            fn producer(pin: &ProducerRef) -> &ProducerRef {
                pin
            }

            async fn subscribe(
                wiring: &StreamWiring<Self>,
                pin: &ProducerRef,
            ) -> Result<Subscription> {
                if wiring.wire.declarations.fails(&pin.instance_id) {
                    return Err(crate::error::Error::Io(std::io::Error::other(
                        "the test refused this declaration",
                    )));
                }
                TopicMessenger::subscribe(
                    &wiring.messenger,
                    &wiring.as_core_node,
                    &wiring.as_instance_id,
                    wiring.wire.target.clone(),
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

        fn target() -> SenderTarget {
            SenderTarget::node(PRODUCER_NODE, "v1").expect("node target")
        }

        fn producer(instance_id: &str) -> ProducerRef {
            ProducerRef::new(CORE, instance_id)
        }

        async fn shared_messenger() -> Arc<Mutex<Messenger>> {
            let mut messenger = Messenger::new(MessengerAdapter::Mock(MockAdapter::default()));
            messenger.start_session().await.expect("mock session");
            Arc::new(Mutex::new(messenger))
        }

        fn wiring(
            shared: &Arc<Mutex<Messenger>>,
            declarations: Arc<Declarations>,
        ) -> StreamWiring<TestSlot> {
            StreamWiring {
                messenger: MessengerHandle::from_shared(Arc::clone(shared)),
                as_core_node: CORE.to_string(),
                as_instance_id: READER.to_string(),
                wire: TestWire {
                    target: target(),
                    declarations,
                },
                topic: TOPIC.to_string(),
                qos: QoSProfile::Reliable,
            }
        }

        async fn publish_from(
            shared: &Arc<Mutex<Messenger>>,
            instance_id: &str,
            body: &'static [u8],
        ) {
            let publisher = TopicMessenger::declare_publisher(
                &MessengerHandle::from_shared(Arc::clone(shared)),
                CORE,
                instance_id,
                target(),
                None,
                TOPIC,
                QoSProfile::Reliable,
            )
            .await
            .expect("declare publisher");
            publisher
                .publish(Payload::from_static(body))
                .await
                .expect("publish");
        }

        /// The first declaration is the caller's to see: a slot whose pin
        /// cannot be declared fails to start.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_first_declaration_that_fails_fails_the_stream() {
            let shared = shared_messenger().await;
            let declarations = Declarations::failing("arm_1", 1);
            let (_state_tx, state_rx) = watch::channel(vec![producer("arm_1")]);

            let outcome =
                start_slot_stream::<TestSlot>(wiring(&shared, Arc::clone(&declarations)), state_rx)
                    .await;

            assert!(outcome.is_err(), "the stream must not start");
            assert_eq!(*declarations.attempts.lock().unwrap(), 1);
        }

        /// A followed pin whose wire channel closes under it is declared
        /// again: the reader rings the converge task, which counts the closed
        /// channel as a declaration it owes.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_followed_pin_whose_wire_channel_closes_is_declared_again() {
            let shared = shared_messenger().await;
            let declarations = Arc::new(Declarations::default());
            let (_state_tx, state_rx) = watch::channel(vec![producer("arm_1")]);
            let mut stream =
                start_slot_stream::<TestSlot>(wiring(&shared, Arc::clone(&declarations)), state_rx)
                    .await
                    .expect("the pin declares");
            assert_eq!(*declarations.attempts.lock().unwrap(), 1);

            // Closing the session closes every wire channel it holds.
            shared
                .lock()
                .await
                .stop_session()
                .await
                .expect("the mock session stops");
            // The reader sees the closed channel on its next poll.
            let _ = tokio::time::timeout(Duration::from_millis(200), stream.next()).await;

            let redeclared = tokio::time::timeout(FIRST_REDECLARE_DELAY * 6, async {
                while *declarations.attempts.lock().unwrap() < 2 {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await;
            assert!(
                redeclared.is_ok(),
                "the closed pin is declared again; attempts={}",
                *declarations.attempts.lock().unwrap()
            );
        }

        /// A pin whose declaration fails later is declared again on the
        /// backoff, and the members that are already declared keep their
        /// subscriptions across the retry.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_failed_declaration_is_retried_while_the_healthy_members_keep_theirs() {
            let shared = shared_messenger().await;
            let declarations = Declarations::failing("arm_2", 1);
            let (state_tx, state_rx) = watch::channel(vec![producer("arm_1")]);
            let mut stream =
                start_slot_stream::<TestSlot>(wiring(&shared, Arc::clone(&declarations)), state_rx)
                    .await
                    .expect("the first pin declares");

            // arm_2 joins the set and its first declaration fails.
            state_tx
                .send(vec![producer("arm_1"), producer("arm_2")])
                .expect("state send");

            // The retry lands within the backoff, so arm_2's publish arrives.
            let heard = tokio::time::timeout(FIRST_REDECLARE_DELAY * 6, async {
                loop {
                    publish_from(&shared, "arm_2", b"from the retried member").await;
                    if let Ok(Some((pin, _))) =
                        tokio::time::timeout(Duration::from_millis(100), stream.next()).await
                    {
                        return pin;
                    }
                }
            })
            .await
            .expect("the failed declaration is retried");
            assert_eq!(*heard, producer("arm_2"));
            assert!(
                *declarations.attempts.lock().unwrap() >= 3,
                "arm_1, then arm_2 failing, then arm_2 again"
            );

            // arm_1 never lost its subscription across the retry. The loop
            // above may have left one more arm_2 message buffered, so read on
            // to arm_1's own payload.
            publish_from(&shared, "arm_1", b"still declared").await;
            let heard = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let (pin, message) = stream.next().await.expect("the stream is open");
                    if &*message.payload_bytes() == b"still declared" {
                        return pin;
                    }
                }
            })
            .await
            .expect("arm_1 still delivers");
            assert_eq!(*heard, producer("arm_1"));
        }

        /// The redeclare schedule on a paused clock: a failed declaration is
        /// tried again after 500 ms, then 1 s, then 2 s, and a declaration
        /// that lands ends the retries.
        #[tokio::test(flavor = "current_thread", start_paused = true)]
        async fn a_failed_declaration_is_retried_on_a_doubling_backoff() {
            let shared = shared_messenger().await;
            let declarations = Declarations::failing("arm_2", 3);
            let (state_tx, state_rx) = watch::channel(vec![producer("arm_1")]);
            let _stream =
                start_slot_stream::<TestSlot>(wiring(&shared, Arc::clone(&declarations)), state_rx)
                    .await
                    .expect("the first pin declares");
            let attempts = || *declarations.attempts.lock().unwrap();
            let settle = || async {
                for _ in 0..16 {
                    tokio::task::yield_now().await;
                }
            };

            state_tx
                .send(vec![producer("arm_1"), producer("arm_2")])
                .expect("state send");
            settle().await;
            assert_eq!(attempts(), 2, "arm_1 at start, then arm_2's first failure");

            for (before, after, expected) in [
                (Duration::from_millis(499), Duration::from_millis(1), 3),
                (Duration::from_millis(999), Duration::from_millis(1), 4),
                (Duration::from_millis(1999), Duration::from_millis(1), 5),
            ] {
                tokio::time::advance(before).await;
                settle().await;
                assert_eq!(
                    attempts(),
                    expected - 1,
                    "no retry before the delay elapses"
                );
                tokio::time::advance(after).await;
                settle().await;
                assert_eq!(attempts(), expected, "one retry as the delay elapses");
            }

            // The third retry landed: nothing is owed, so the clock can run
            // on without another attempt.
            tokio::time::advance(Duration::from_secs(60)).await;
            settle().await;
            assert_eq!(attempts(), 5, "a declaration that lands ends the retries");
        }

        /// The window the stale filter exists for: the slot's state drops a
        /// member while a message it published is still buffered, and the
        /// converge task has not yet run. The reader must not surface it.
        #[tokio::test(flavor = "current_thread")]
        async fn a_message_buffered_under_a_departed_pin_is_never_read() {
            let shared = shared_messenger().await;
            let declarations = Arc::new(Declarations::default());
            let (state_tx, state_rx) = watch::channel(vec![producer("arm_1")]);
            let mut stream =
                start_slot_stream::<TestSlot>(wiring(&shared, Arc::clone(&declarations)), state_rx)
                    .await
                    .expect("the pin declares");
            publish_from(&shared, "arm_1", b"buffered before leaving").await;

            // No await between dropping the member and the read, so the
            // converge task cannot have run: only the read-time filter can
            // keep this message from surfacing.
            state_tx.send(Vec::new()).expect("state send");
            let outcome = tokio::time::timeout(Duration::from_millis(300), stream.next()).await;

            assert!(
                outcome.is_err(),
                "a departed member's buffered message must never surface"
            );
        }
    }
}
