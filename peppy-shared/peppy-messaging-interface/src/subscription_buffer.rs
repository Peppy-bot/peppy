//! The buffer between a subscription's transport callback and its reader.
//!
//! The transport hands every received message to a [`SubscriptionSink`], and
//! the reader drains the paired `flume::Receiver`. The sink decides what
//! happens to a message that arrives while the reader is behind:
//!
//! - A topic subscription with backpressure waits for room
//!   ([`TopicBuffer::Backpressure`]). The transport's reception waits with it,
//!   so the stall reaches the publisher. That wait holds up every message
//!   that arrives over the same connection, not only the topic's.
//! - A keep-latest topic subscription never waits
//!   ([`TopicBuffer::KeepLatest`]). It drops its oldest unread message to make
//!   room, so a reader that reads late, or never, holds up no other message.
//! - A goal's feedback stream never waits ([`Buffering::Feedback`]). Its
//!   reader often awaits the goal's result, which arrives over the same
//!   connection, so a wait there would hold up the very reply the reader
//!   waits for. It keeps every message up to its bound of unread messages,
//!   and overflows past it ([`FeedbackBuffer`]).

use crate::types::{FeedbackBuffer, FeedbackOverflow, TopicBuffer, TopicMessage};
use config::node::TopicRetention;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// What a subscription does with a message that arrives while its reader is
/// behind.
#[derive(Debug, Clone)]
pub(crate) enum Buffering {
    /// A topic subscription, held as its subscriber asked.
    Topic(TopicBuffer),
    /// A goal's feedback stream, held as the goal's caller asked.
    Feedback(FeedbackBuffer),
}

/// A topic subscription's queue capacity: its QoS tier's size, raised to the
/// topic's retention depth so one publisher's messages replayed at a join fit
/// before the reader takes one.
pub(crate) fn queue_capacity(tier_size: usize, retention: TopicRetention) -> usize {
    match retention {
        TopicRetention::LiveOnly => tier_size,
        TopicRetention::Latest { depth } => tier_size.max(depth.get()),
    }
}

/// Opens a subscription's buffer: the sink the transport fills and the
/// receiver the reader drains. `capacity` bounds a topic's queue; a feedback
/// buffer takes its own bound. `keyexpr` names the subscription in the log.
pub(crate) fn subscription_channel(
    keyexpr: &str,
    capacity: usize,
    buffering: Buffering,
) -> (SubscriptionSink, flume::Receiver<TopicMessage>) {
    match buffering {
        Buffering::Topic(TopicBuffer::Backpressure) => {
            let (tx, rx) = flume::bounded(capacity);
            (SubscriptionSink::Backpressure(tx), rx)
        }
        Buffering::Topic(TopicBuffer::KeepLatest) => {
            let (queue, rx) = LatestQueue::open(keyexpr, capacity);
            (SubscriptionSink::KeepLatest(Arc::new(queue)), rx)
        }
        Buffering::Feedback(buffer) => {
            let (sink, rx) = FeedbackSink::open(keyexpr, buffer);
            (SubscriptionSink::Feedback(Arc::new(sink)), rx)
        }
    }
}

/// The transport's side of a subscription buffer. Cloning it shares the same
/// buffer.
#[derive(Clone)]
pub(crate) enum SubscriptionSink {
    /// A topic's bounded queue: a message waits while the queue is full.
    Backpressure(flume::Sender<TopicMessage>),
    /// A topic's bounded queue that drops its oldest unread message when full.
    KeepLatest(Arc<LatestQueue>),
    /// A goal's feedback stream, which never waits.
    Feedback(Arc<FeedbackSink>),
}

impl SubscriptionSink {
    /// Hands `message` to the reader from a transport callback. The callback
    /// runs on the transport's reception thread, so a full backpressure queue
    /// blocks that thread until the reader takes a message. A keep-latest
    /// queue and a feedback stream never block it. A message for a reader
    /// that is gone is discarded.
    pub(crate) fn deliver(&self, message: TopicMessage) {
        match self {
            Self::Backpressure(tx) => {
                let _ = tx.send(message);
            }
            Self::KeepLatest(queue) => queue.push(message),
            Self::Feedback(sink) => sink.deliver(message),
        }
    }

    /// [`Self::deliver`] for a publisher that runs as a task (the mock
    /// adapter): a full backpressure queue suspends the publishing task, not a
    /// thread.
    pub(crate) async fn deliver_async(&self, message: TopicMessage) {
        match self {
            Self::Backpressure(tx) => {
                let _ = tx.send_async(message).await;
            }
            Self::KeepLatest(queue) => queue.push(message),
            Self::Feedback(sink) => sink.deliver(message),
        }
    }

    /// Whether the reader dropped its receiver.
    pub(crate) fn reader_gone(&self) -> bool {
        match self {
            Self::Backpressure(tx) => tx.is_disconnected(),
            Self::KeepLatest(queue) => queue.reader_gone(),
            Self::Feedback(sink) => sink.reader_gone(),
        }
    }
}

/// A bounded topic queue that never makes its sender wait: a message that
/// arrives while the queue is full drops the oldest unread one. The first
/// drop of a queue is logged once, with the keyexpr of its subscription.
pub(crate) struct LatestQueue {
    tx: flume::Sender<TopicMessage>,
    /// A receiver of the queue's own, which takes the oldest unread message
    /// out to make room.
    oldest: flume::Receiver<TopicMessage>,
    /// Set at the first message the queue drops, so the log names a
    /// subscription once.
    dropped_any: AtomicBool,
    keyexpr: String,
}

impl LatestQueue {
    /// Opens the queue and the receiver its reader drains.
    fn open(keyexpr: &str, capacity: usize) -> (Self, flume::Receiver<TopicMessage>) {
        // At least one slot: a channel with none only hands a message to a
        // reader that waits at that moment, so dropping the oldest message
        // could never make room.
        let (tx, rx) = flume::bounded(capacity.max(1));
        let queue = Self {
            tx,
            oldest: rx.clone(),
            dropped_any: AtomicBool::new(false),
            keyexpr: keyexpr.to_string(),
        };
        (queue, rx)
    }

    /// Queues `message`, dropping the oldest unread message while the queue
    /// is full. A message for a reader that is gone is discarded.
    fn push(&self, message: TopicMessage) {
        if self.reader_gone() {
            return;
        }
        let mut message = message;
        loop {
            match self.tx.try_send(message) {
                Ok(()) | Err(flume::TrySendError::Disconnected(_)) => return,
                Err(flume::TrySendError::Full(returned)) => {
                    message = returned;
                    // Drops the oldest message still unread. When the reader
                    // emptied the queue since the try, nothing is left to
                    // drop and the next try finds room.
                    if self.oldest.try_recv().is_ok() {
                        self.note_dropped();
                    }
                }
            }
        }
    }

    fn note_dropped(&self) {
        if self.dropped_any.swap(true, Ordering::Relaxed) {
            return;
        }
        tracing::debug!(
            keyexpr = %self.keyexpr,
            capacity = self.tx.capacity(),
            "the reader of this subscription is behind; the oldest unread messages are dropped",
        );
    }

    /// Whether the reader dropped its receiver. The queue holds a receiver of
    /// its own, so the reader is gone once that one is the last.
    fn reader_gone(&self) -> bool {
        self.tx.receiver_count() <= 1
    }
}

/// A goal's feedback buffer. An empty payload is the end of the stream (the
/// message `publish_end` sends in peppylib): the buffer always keeps it and
/// takes nothing after it, even when a worker publishes more feedback after
/// its goal ended.
///
/// Every other message stays until the reader takes it, up to the bound of
/// unread messages. A message that arrives while `bound` messages wait
/// overflows the buffer: it drops every unread message, takes nothing more,
/// and raises its [`FeedbackOverflow`].
pub(crate) struct FeedbackSink {
    /// Unbounded, so a send never waits: the bound is checked before it.
    tx: flume::Sender<TopicMessage>,
    /// A receiver of the buffer's own, which drops the unread messages at an
    /// overflow.
    unread: flume::Receiver<TopicMessage>,
    bound: usize,
    overflow: FeedbackOverflow,
    /// Each delivery holds this lock from its check to its push, so no
    /// message lands after the end or an overflow while another delivery
    /// takes it. The lock only waits for another delivery, which never waits
    /// for the reader.
    state: Mutex<FeedbackState>,
    keyexpr: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeedbackState {
    Open,
    Ended,
    Overflowed,
}

impl FeedbackSink {
    /// Opens the buffer and the receiver its reader drains.
    fn open(keyexpr: &str, buffer: FeedbackBuffer) -> (Self, flume::Receiver<TopicMessage>) {
        let (tx, rx) = flume::unbounded();
        let sink = Self {
            tx,
            unread: rx.clone(),
            bound: buffer.bound().get(),
            overflow: buffer.overflow(),
            state: Mutex::new(FeedbackState::Open),
            keyexpr: keyexpr.to_string(),
        };
        (sink, rx)
    }

    fn deliver(&self, message: TopicMessage) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if *state != FeedbackState::Open || self.reader_gone() {
            return;
        }
        if message.payload().is_empty() {
            *state = FeedbackState::Ended;
            let _ = self.tx.send(message);
            return;
        }
        if self.tx.len() >= self.bound {
            *state = FeedbackState::Overflowed;
            self.unread.drain().for_each(drop);
            tracing::debug!(
                keyexpr = %self.keyexpr,
                bound = self.bound,
                "the reader of this feedback stream fell behind by more than its bound; \
                 the stream overflowed",
            );
            self.overflow.raise();
            return;
        }
        let _ = self.tx.send(message);
    }

    /// Whether the reader dropped its receiver. The buffer holds a receiver
    /// of its own, so the reader is gone once that one is the last.
    fn reader_gone(&self) -> bool {
        self.tx.receiver_count() <= 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYEXPR: &str = "test/subscription";

    fn message(index: usize) -> TopicMessage {
        TopicMessage::from_parts(
            "core".to_string(),
            "instance".to_string(),
            bytes::Bytes::from(format!("message-{index}")),
        )
    }

    fn end_of_stream() -> TopicMessage {
        TopicMessage::from_parts(
            "core".to_string(),
            "instance".to_string(),
            bytes::Bytes::new(),
        )
    }

    fn feedback_channel(
        bound: usize,
    ) -> (
        SubscriptionSink,
        flume::Receiver<TopicMessage>,
        FeedbackOverflow,
    ) {
        let buffer = FeedbackBuffer::new(crate::FeedbackBound::new(
            std::num::NonZeroUsize::new(bound).expect("a bound of one message or more"),
        ));
        let overflow = buffer.overflow();
        let (sink, rx) = subscription_channel(KEYEXPR, 0, Buffering::Feedback(buffer));
        (sink, rx, overflow)
    }

    fn keep_latest_topic_channel(
        capacity: usize,
    ) -> (SubscriptionSink, flume::Receiver<TopicMessage>) {
        subscription_channel(KEYEXPR, capacity, Buffering::Topic(TopicBuffer::KeepLatest))
    }

    /// The payloads waiting in `rx`, in the order the reader gets them.
    fn drain(rx: &flume::Receiver<TopicMessage>) -> Vec<String> {
        rx.try_iter()
            .map(|message| String::from_utf8_lossy(&message.payload().as_bytes()).into_owned())
            .collect()
    }

    fn names(indexes: impl IntoIterator<Item = usize>) -> Vec<String> {
        indexes
            .into_iter()
            .map(|index| format!("message-{index}"))
            .collect()
    }

    #[test]
    fn a_retaining_subscription_queue_fits_the_retention_depth() {
        use crate::declared::test_support::latest;
        assert_eq!(queue_capacity(128, TopicRetention::LiveOnly), 128);
        assert_eq!(queue_capacity(128, latest(1)), 128);
        assert_eq!(queue_capacity(128, latest(300)), 300);
    }

    /// The deepest retention fits the largest default queue, so a retaining
    /// subscription's queue stays within the default sizes.
    #[test]
    fn the_largest_default_queue_holds_the_deepest_retention() {
        assert!(
            crate::SubscriberBufferSizes::default().high_throughput
                >= config::node::MAX_RETENTION_DEPTH
        );
    }

    #[test]
    fn feedback_keeps_every_message_up_to_its_bound() {
        let (sink, rx, overflow) = feedback_channel(3);
        for index in 0..3 {
            sink.deliver(message(index));
        }
        assert_eq!(drain(&rx), names(0..3));
        assert!(!overflow.is_raised());
    }

    #[test]
    fn feedback_keeps_room_the_reader_made() {
        let (sink, rx, overflow) = feedback_channel(2);
        sink.deliver(message(0));
        sink.deliver(message(1));
        assert_eq!(drain(&rx), names(0..2));
        sink.deliver(message(2));
        sink.deliver(message(3));
        assert_eq!(drain(&rx), names(2..4));
        assert!(!overflow.is_raised());
    }

    #[test]
    fn a_message_past_the_bound_overflows_the_feedback() {
        let (sink, rx, overflow) = feedback_channel(2);
        for index in 0..3 {
            sink.deliver(message(index));
        }
        assert!(overflow.is_raised(), "the third unread message overflows");
        assert_eq!(
            drain(&rx),
            Vec::<String>::new(),
            "the unread messages are dropped"
        );
        sink.deliver(message(3));
        sink.deliver(end_of_stream());
        assert_eq!(
            drain(&rx),
            Vec::<String>::new(),
            "nothing lands after an overflow"
        );
    }

    #[test]
    fn the_end_of_the_stream_never_overflows_the_feedback() {
        let (sink, rx, overflow) = feedback_channel(2);
        sink.deliver(message(0));
        sink.deliver(message(1));
        sink.deliver(end_of_stream());
        assert!(!overflow.is_raised());
        let mut expected = names(0..2);
        expected.push(String::new());
        assert_eq!(drain(&rx), expected);
    }

    #[test]
    fn feedback_takes_nothing_after_the_end_of_the_stream() {
        let (sink, rx, _overflow) = feedback_channel(4);
        sink.deliver(message(0));
        sink.deliver(end_of_stream());
        // Feedback a worker publishes after its goal ended.
        sink.deliver(message(1));
        assert_eq!(drain(&rx), vec!["message-0".to_string(), String::new()]);
    }

    #[tokio::test]
    async fn an_overflow_wakes_its_waiter() {
        let (sink, _rx, overflow) = feedback_channel(1);
        let mut raised = std::pin::pin!(overflow.raised());
        assert!(poll_once(raised.as_mut()).is_pending());
        sink.deliver(message(0));
        sink.deliver(message(1));
        assert!(poll_once(raised.as_mut()).is_ready());
    }

    /// Workers publish feedback on several threads while one of them ends
    /// the stream: whatever the interleaving, the end is kept and is the last
    /// message.
    #[test]
    fn a_concurrent_end_is_kept_last() {
        const PUBLISHERS: usize = 4;
        const MESSAGES: usize = 200;
        let (sink, rx, _overflow) = feedback_channel(PUBLISHERS * MESSAGES);
        let start = std::sync::Barrier::new(PUBLISHERS + 1);
        std::thread::scope(|scope| {
            for _ in 0..PUBLISHERS {
                scope.spawn(|| {
                    start.wait();
                    for index in 0..MESSAGES {
                        sink.deliver(message(index));
                    }
                });
            }
            start.wait();
            sink.deliver(end_of_stream());
        });
        assert_eq!(
            drain(&rx).last(),
            Some(&String::new()),
            "the end must be the last message"
        );
    }

    #[test]
    fn a_feedback_sink_never_waits_for_its_reader() {
        let (sink, rx, _overflow) = feedback_channel(8);
        for index in 0..3 {
            let mut delivery = std::pin::pin!(sink.deliver_async(message(index)));
            assert!(
                poll_once(delivery.as_mut()).is_ready(),
                "feedback {index} must not wait for the reader"
            );
        }
        assert_eq!(drain(&rx), names(0..3));
    }

    #[test]
    fn a_feedback_sink_sees_its_reader_go() {
        let (sink, rx, overflow) = feedback_channel(1);
        assert!(!sink.reader_gone(), "the reader is still there");
        drop(rx);
        assert!(sink.reader_gone(), "the reader dropped its receiver");
        // Delivering to a gone reader discards the message and returns, and
        // never overflows.
        sink.deliver(message(0));
        sink.deliver(message(1));
        assert!(!overflow.is_raised());
    }

    #[test]
    fn a_topic_sink_sees_its_reader_go() {
        let (sink, rx) =
            subscription_channel(KEYEXPR, 1, Buffering::Topic(TopicBuffer::Backpressure));
        assert!(!sink.reader_gone());
        drop(rx);
        assert!(sink.reader_gone());
    }

    /// Polls `future` once, without a runtime.
    fn poll_once<F: std::future::Future>(future: std::pin::Pin<&mut F>) -> std::task::Poll<()> {
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        future.poll(&mut context).map(|_| ())
    }

    #[test]
    fn a_full_topic_queue_waits_for_its_reader() {
        let (sink, rx) =
            subscription_channel(KEYEXPR, 1, Buffering::Topic(TopicBuffer::Backpressure));
        sink.deliver(message(0));
        let mut second = std::pin::pin!(sink.deliver_async(message(1)));
        assert!(
            poll_once(second.as_mut()).is_pending(),
            "a full topic queue must wait for its reader"
        );
        let first = rx.try_recv().expect("the first message is queued");
        assert_eq!(first.payload().as_bytes().as_ref(), b"message-0");
        assert!(
            poll_once(second.as_mut()).is_ready(),
            "the reader made room"
        );
        assert_eq!(drain(&rx), names([1]));
    }

    #[test]
    fn a_keep_latest_topic_drops_the_oldest_unread_message_when_full() {
        let (sink, rx) = keep_latest_topic_channel(3);
        for index in 0..5 {
            sink.deliver(message(index));
        }
        assert_eq!(drain(&rx), names(2..5));
    }

    #[test]
    fn a_keep_latest_topic_keeps_room_the_reader_made() {
        let (sink, rx) = keep_latest_topic_channel(2);
        sink.deliver(message(0));
        sink.deliver(message(1));
        let first = rx.try_recv().expect("the first message is queued");
        assert_eq!(first.payload().as_bytes().as_ref(), b"message-0");
        // The reader took one message, so the next one fits without a drop.
        sink.deliver(message(2));
        assert_eq!(drain(&rx), names(1..3));
    }

    #[test]
    fn a_keep_latest_topic_never_waits_for_its_reader() {
        let (sink, rx) = keep_latest_topic_channel(1);
        for index in 0..3 {
            let mut delivery = std::pin::pin!(sink.deliver_async(message(index)));
            assert!(
                poll_once(delivery.as_mut()).is_ready(),
                "message {index} must not wait for the reader"
            );
        }
        assert_eq!(drain(&rx), names([2]));
    }

    #[test]
    fn a_keep_latest_topic_sees_its_reader_go() {
        let (sink, rx) = keep_latest_topic_channel(1);
        assert!(!sink.reader_gone(), "the reader is still there");
        drop(rx);
        assert!(sink.reader_gone(), "the reader dropped its receiver");
        // Delivering to a gone reader discards the message and returns.
        sink.deliver(message(0));
        sink.deliver(message(1));
    }

    /// An empty payload ends a feedback stream, but it is an ordinary message
    /// on a topic: the topic takes the messages that follow it.
    #[test]
    fn a_keep_latest_topic_has_no_end_of_stream() {
        let (sink, rx) = keep_latest_topic_channel(3);
        sink.deliver(message(0));
        sink.deliver(end_of_stream());
        sink.deliver(message(1));
        assert_eq!(
            drain(&rx),
            vec![
                "message-0".to_string(),
                String::new(),
                "message-1".to_string()
            ]
        );
    }

    /// Several transport threads deliver to one keep-latest topic at once,
    /// and nothing reads it. Whatever the interleaving, the queue ends full,
    /// and the messages it kept from each thread are in that thread's order.
    #[test]
    fn concurrent_deliveries_leave_a_full_queue_in_each_threads_order() {
        const PUBLISHERS: usize = 4;
        const MESSAGES: usize = 200;
        const CAPACITY: usize = 16;
        let (sink, rx) = keep_latest_topic_channel(CAPACITY);
        let start = std::sync::Barrier::new(PUBLISHERS);
        std::thread::scope(|scope| {
            for publisher in 0..PUBLISHERS {
                let (sink, start) = (&sink, &start);
                scope.spawn(move || {
                    start.wait();
                    for index in 0..MESSAGES {
                        sink.deliver(tagged(publisher, index));
                    }
                });
            }
        });
        let kept = drain(&rx);
        assert_eq!(kept.len(), CAPACITY, "the queue ends full: {kept:?}");
        for publisher in 0..PUBLISHERS {
            let prefix = format!("publisher-{publisher}-");
            let indexes: Vec<usize> = kept
                .iter()
                .filter_map(|payload| payload.strip_prefix(&prefix)?.parse().ok())
                .collect();
            assert!(
                indexes.windows(2).all(|pair| pair[0] < pair[1]),
                "publisher {publisher}'s messages keep their order: {kept:?}"
            );
        }
    }

    fn tagged(publisher: usize, index: usize) -> TopicMessage {
        TopicMessage::from_parts(
            "core".to_string(),
            "instance".to_string(),
            bytes::Bytes::from(format!("publisher-{publisher}-{index}")),
        )
    }
}
