//! The buffer between a subscription's transport callback and its reader.
//!
//! The transport hands every received message to a [`SubscriptionSink`], and
//! the reader drains the paired `flume::Receiver`. The sink decides what
//! happens to a message that arrives while the reader is behind:
//!
//! - A topic waits for room ([`Buffering::Backpressure`]). The transport's
//!   reception waits with it, so the stall reaches the publisher. That wait
//!   holds up every message that arrives over the same connection, not only
//!   the topic's.
//! - A goal's feedback stream never waits ([`Buffering::Feedback`]). Its
//!   reader often awaits the goal's result, which arrives over the same
//!   connection, so a wait there would hold up the very reply the reader
//!   waits for.

use crate::types::{FeedbackBuffer, TopicMessage};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// What a subscription does with a message that arrives while its reader is
/// behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Buffering {
    /// Wait until the reader takes a message: topics.
    Backpressure,
    /// A goal's feedback stream, held as the goal's caller asked.
    Feedback(FeedbackBuffer),
}

/// Opens a subscription's buffer: the sink the transport fills and the
/// receiver the reader drains. `capacity` bounds a topic's queue and a
/// keep-latest feedback buffer; a keep-all feedback buffer has no bound.
/// `keyexpr` names the subscription in the log.
pub(crate) fn subscription_channel(
    keyexpr: &str,
    capacity: usize,
    buffering: Buffering,
) -> (SubscriptionSink, flume::Receiver<TopicMessage>) {
    let feedback = match buffering {
        Buffering::Backpressure => {
            let (tx, rx) = flume::bounded(capacity);
            return (SubscriptionSink::Topic(tx), rx);
        }
        Buffering::Feedback(feedback) => feedback,
    };
    let (tx, rx, keep) = match feedback {
        FeedbackBuffer::KeepLatest => {
            // At least one slot: a channel with none only hands a message to
            // a reader that waits at that moment, so dropping the oldest
            // message could never make room.
            let (tx, rx) = flume::bounded(capacity.max(1));
            let oldest = rx.clone();
            (tx, rx, Keep::Latest { oldest })
        }
        FeedbackBuffer::KeepAll => {
            let (tx, rx) = flume::unbounded();
            (tx, rx, Keep::All)
        }
    };
    let sink = FeedbackSink {
        tx,
        keep,
        ended: Mutex::new(false),
        dropped_any: AtomicBool::new(false),
        keyexpr: keyexpr.to_string(),
    };
    (SubscriptionSink::Feedback(Arc::new(sink)), rx)
}

/// The transport's side of a subscription buffer. Cloning it shares the same
/// buffer.
#[derive(Clone)]
pub(crate) enum SubscriptionSink {
    /// A topic's bounded queue: a message waits while the queue is full.
    Topic(flume::Sender<TopicMessage>),
    /// A goal's feedback stream, which never waits.
    Feedback(Arc<FeedbackSink>),
}

impl SubscriptionSink {
    /// Hands `message` to the reader from a transport callback. The callback
    /// runs on the transport's reception thread, so a full topic queue blocks
    /// that thread until the reader takes a message. A feedback stream never
    /// blocks it. A message for a reader that is gone is discarded.
    pub(crate) fn deliver(&self, message: TopicMessage) {
        match self {
            Self::Topic(tx) => {
                let _ = tx.send(message);
            }
            Self::Feedback(sink) => sink.deliver(message),
        }
    }

    /// [`Self::deliver`] for a publisher that runs as a task (the mock
    /// adapter): a full topic queue suspends the publishing task, not a thread.
    pub(crate) async fn deliver_async(&self, message: TopicMessage) {
        match self {
            Self::Topic(tx) => {
                let _ = tx.send_async(message).await;
            }
            Self::Feedback(sink) => sink.deliver(message),
        }
    }

    /// Whether the reader dropped its receiver.
    pub(crate) fn reader_gone(&self) -> bool {
        match self {
            Self::Topic(tx) => tx.is_disconnected(),
            Self::Feedback(sink) => sink.reader_gone(),
        }
    }
}

/// A goal's feedback buffer. An empty payload is the end of the stream (the
/// message `publish_end` sends in peppylib): the buffer keeps it and takes
/// nothing after it, so a keep-latest buffer never drops the end, even when a
/// worker publishes more feedback after its goal ended.
pub(crate) struct FeedbackSink {
    tx: flume::Sender<TopicMessage>,
    keep: Keep,
    /// Whether the end of the stream is buffered. Each delivery holds this
    /// lock from its check to its push, so no message lands after the end
    /// while another delivery buffers it. The lock only waits for another
    /// delivery, which never waits for the reader.
    ended: Mutex<bool>,
    /// Set at the first message a keep-latest buffer drops, so the log names
    /// a stream once.
    dropped_any: AtomicBool,
    keyexpr: String,
}

enum Keep {
    /// Bounded: a message that arrives while the buffer is full drops the
    /// oldest unread one. `oldest` is a receiver of the sink's own, which
    /// takes that message out.
    Latest {
        oldest: flume::Receiver<TopicMessage>,
    },
    /// Unbounded: every message stays until the reader takes it.
    All,
}

impl FeedbackSink {
    fn deliver(&self, message: TopicMessage) {
        let mut ended = self.ended.lock().unwrap_or_else(PoisonError::into_inner);
        if *ended || self.reader_gone() {
            return;
        }
        *ended = message.payload().is_empty();
        match &self.keep {
            // An unbounded channel is never full, so this send never waits.
            Keep::All => {
                let _ = self.tx.send(message);
            }
            Keep::Latest { oldest } => self.push_dropping_oldest(oldest, message),
        }
    }

    fn push_dropping_oldest(&self, oldest: &flume::Receiver<TopicMessage>, message: TopicMessage) {
        let mut message = message;
        loop {
            match self.tx.try_send(message) {
                Ok(()) | Err(flume::TrySendError::Disconnected(_)) => return,
                Err(flume::TrySendError::Full(returned)) => {
                    message = returned;
                    // Drops the oldest message still unread. When the reader
                    // emptied the buffer since the try, nothing is left to
                    // drop and the next try finds room.
                    if oldest.try_recv().is_ok() {
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
            "the reader of this feedback stream is behind; the oldest unread messages are dropped",
        );
    }

    fn reader_gone(&self) -> bool {
        match self.keep {
            // A keep-latest buffer holds a receiver of its own, so the reader
            // is gone once that one is the last.
            Keep::Latest { .. } => self.tx.receiver_count() <= 1,
            Keep::All => self.tx.is_disconnected(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYEXPR: &str = "test/feedback";

    fn feedback(index: usize) -> TopicMessage {
        TopicMessage::from_parts(
            "core".to_string(),
            "instance".to_string(),
            bytes::Bytes::from(format!("feedback-{index}")),
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
        capacity: usize,
        buffer: FeedbackBuffer,
    ) -> (SubscriptionSink, flume::Receiver<TopicMessage>) {
        subscription_channel(KEYEXPR, capacity, Buffering::Feedback(buffer))
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
            .map(|index| format!("feedback-{index}"))
            .collect()
    }

    #[test]
    fn keep_latest_drops_the_oldest_unread_message_when_full() {
        let (sink, rx) = feedback_channel(3, FeedbackBuffer::KeepLatest);
        for index in 0..5 {
            sink.deliver(feedback(index));
        }
        assert_eq!(drain(&rx), names(2..5));
    }

    #[test]
    fn keep_latest_keeps_room_the_reader_made() {
        let (sink, rx) = feedback_channel(2, FeedbackBuffer::KeepLatest);
        sink.deliver(feedback(0));
        sink.deliver(feedback(1));
        assert_eq!(drain(&rx), names(0..2));
        sink.deliver(feedback(2));
        sink.deliver(feedback(3));
        assert_eq!(drain(&rx), names(2..4));
    }

    #[test]
    fn keep_latest_never_drops_the_end_of_the_stream() {
        let (sink, rx) = feedback_channel(2, FeedbackBuffer::KeepLatest);
        for index in 0..3 {
            sink.deliver(feedback(index));
        }
        sink.deliver(end_of_stream());
        // Feedback a worker publishes after its goal ended.
        for index in 3..6 {
            sink.deliver(feedback(index));
        }
        let mut expected = names([2]);
        expected.push(String::new());
        assert_eq!(drain(&rx), expected);
    }

    #[test]
    fn keep_latest_with_no_capacity_keeps_the_newest_message() {
        let (sink, rx) = feedback_channel(0, FeedbackBuffer::KeepLatest);
        for index in 0..3 {
            sink.deliver(feedback(index));
        }
        assert_eq!(drain(&rx), names([2]));
        sink.deliver(feedback(3));
        sink.deliver(end_of_stream());
        assert_eq!(drain(&rx), vec![String::new()]);
    }

    /// Workers publish feedback on several threads while one of them ends
    /// the stream: whatever the interleaving, the end is kept and is the last
    /// message.
    #[test]
    fn a_concurrent_end_is_kept_last() {
        const PUBLISHERS: usize = 4;
        const MESSAGES: usize = 200;
        for buffer in [FeedbackBuffer::KeepLatest, FeedbackBuffer::KeepAll] {
            let (sink, rx) = feedback_channel(1, buffer);
            let start = std::sync::Barrier::new(PUBLISHERS + 1);
            std::thread::scope(|scope| {
                for _ in 0..PUBLISHERS {
                    scope.spawn(|| {
                        start.wait();
                        for index in 0..MESSAGES {
                            sink.deliver(feedback(index));
                        }
                    });
                }
                start.wait();
                sink.deliver(end_of_stream());
            });
            assert_eq!(
                drain(&rx).last(),
                Some(&String::new()),
                "{buffer:?}: the end must be the last message"
            );
        }
    }

    #[test]
    fn keep_all_keeps_every_message_past_the_capacity() {
        let (sink, rx) = feedback_channel(2, FeedbackBuffer::KeepAll);
        for index in 0..10 {
            sink.deliver(feedback(index));
        }
        assert_eq!(drain(&rx), names(0..10));
    }

    #[test]
    fn keep_all_takes_nothing_after_the_end_of_the_stream() {
        let (sink, rx) = feedback_channel(2, FeedbackBuffer::KeepAll);
        sink.deliver(feedback(0));
        sink.deliver(end_of_stream());
        sink.deliver(feedback(1));
        assert_eq!(drain(&rx), vec!["feedback-0".to_string(), String::new()]);
    }

    #[test]
    fn a_feedback_sink_never_waits_for_its_reader() {
        for buffer in [FeedbackBuffer::KeepLatest, FeedbackBuffer::KeepAll] {
            let (sink, rx) = feedback_channel(1, buffer);
            for index in 0..3 {
                let mut delivery = std::pin::pin!(sink.deliver_async(feedback(index)));
                assert!(
                    poll_once(delivery.as_mut()).is_ready(),
                    "{buffer:?}: feedback {index} must not wait for the reader"
                );
            }
            assert_eq!(drain(&rx).last(), Some(&"feedback-2".to_string()));
        }
    }

    #[test]
    fn a_feedback_sink_sees_its_reader_go() {
        for buffer in [FeedbackBuffer::KeepLatest, FeedbackBuffer::KeepAll] {
            let (sink, rx) = feedback_channel(1, buffer);
            assert!(!sink.reader_gone(), "{buffer:?}: the reader is still there");
            drop(rx);
            assert!(
                sink.reader_gone(),
                "{buffer:?}: the reader dropped its receiver"
            );
            // Delivering to a gone reader discards the message and returns.
            sink.deliver(feedback(0));
            sink.deliver(feedback(1));
        }
    }

    #[test]
    fn a_topic_sink_sees_its_reader_go() {
        let (sink, rx) = subscription_channel(KEYEXPR, 1, Buffering::Backpressure);
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
        let (sink, rx) = subscription_channel(KEYEXPR, 1, Buffering::Backpressure);
        sink.deliver(feedback(0));
        let mut second = std::pin::pin!(sink.deliver_async(feedback(1)));
        assert!(
            poll_once(second.as_mut()).is_pending(),
            "a full topic queue must wait for its reader"
        );
        let first = rx.try_recv().expect("the first message is queued");
        assert_eq!(first.payload().as_bytes().as_ref(), b"feedback-0");
        assert!(
            poll_once(second.as_mut()).is_ready(),
            "the reader made room"
        );
        assert_eq!(drain(&rx), names([1]));
    }
}
