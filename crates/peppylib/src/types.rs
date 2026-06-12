pub use core_node_api::Payload;

/// A borrowed view of a received message's payload, returned by
/// [`Message::payload`]. Dereferences to `[u8]` over the transport's buffer
/// when it is contiguous — always the case for shared-memory deliveries,
/// where that is the producer's physical memory, mapped, with no copy on the
/// access path. A network payload fragmented across transport batches is
/// materialized once when the view is created. The underlying buffer is kept
/// alive by the [`Message`] the view borrows from.
///
/// Call [`to_owned`](Self::to_owned) for an owned [`Payload`] when the bytes
/// must outlive the message. Don't hoard views (or the messages backing
/// them): a held view pins its shared-memory buffer, and a subscriber that
/// accumulates them can starve the publisher's pool into the (slower)
/// copying fallback.
pub struct PayloadView<'a>(std::borrow::Cow<'a, [u8]>);

impl PayloadView<'_> {
    /// An owned copy of the payload bytes, for callers that need ownership.
    #[allow(clippy::wrong_self_convention)]
    pub fn to_owned(&self) -> Payload {
        Payload::from(bytes::Bytes::copy_from_slice(&self.0))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::ops::Deref for PayloadView<'_> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<[u8]> for PayloadView<'_> {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for PayloadView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PayloadView({} bytes)", self.0.len())
    }
}

// Comparison ergonomics for the common assertion shapes:
// `assert_eq!(msg.payload(), &expected_payload)` and friends.
impl PartialEq<Payload> for PayloadView<'_> {
    fn eq(&self, other: &Payload) -> bool {
        *self.0 == **other
    }
}

impl PartialEq<&Payload> for PayloadView<'_> {
    fn eq(&self, other: &&Payload) -> bool {
        *self.0 == ***other
    }
}

impl PartialEq<PayloadView<'_>> for Payload {
    fn eq(&self, other: &PayloadView<'_>) -> bool {
        **self == *other.0
    }
}

impl PartialEq<[u8]> for PayloadView<'_> {
    fn eq(&self, other: &[u8]) -> bool {
        *self.0 == *other
    }
}

impl PartialEq<&[u8]> for PayloadView<'_> {
    fn eq(&self, other: &&[u8]) -> bool {
        *self.0 == **other
    }
}

impl<const N: usize> PartialEq<&[u8; N]> for PayloadView<'_> {
    fn eq(&self, other: &&[u8; N]) -> bool {
        *self.0 == other[..]
    }
}

impl PartialEq<Vec<u8>> for PayloadView<'_> {
    fn eq(&self, other: &Vec<u8>) -> bool {
        *self.0 == *other.as_slice()
    }
}

/// A wrapper around `pmi::TopicMessage` to abstract away the underlying message implementation.
pub struct Message(pub(crate) pmi::TopicMessage);

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Message")
            .field("instance_id", &self.instance_id())
            .field("core_node", &self.core_node())
            // Length, not the view: debug-formatting must not materialize a
            // fragmented payload just to print its size.
            .field("payload_len", &self.0.payload().len())
            .finish()
    }
}

impl Message {
    /// Get the payload of the message as a borrowed [`PayloadView`] over the
    /// transport's buffer — zero-copy for shared-memory deliveries and
    /// contiguous network payloads (a fragmented network payload is
    /// materialized once). Use [`PayloadView::to_owned`] when ownership is
    /// needed.
    pub fn payload(&self) -> PayloadView<'_> {
        PayloadView(self.0.payload().as_bytes())
    }

    /// Whether the payload bytes live in a shared-memory segment (the
    /// producer wrote them once; [`Self::payload`] reads the same physical
    /// memory). Introspection for the transport matrix tests and the latency
    /// bench's environment line.
    pub fn payload_is_shm_backed(&self) -> bool {
        self.0.payload().is_shm_backed()
    }

    /// The payload length in bytes, without materializing a fragmented
    /// network payload the way [`Self::payload`] would. The stack benchmark
    /// sizes delivery samples with it on every message.
    pub fn payload_len(&self) -> usize {
        self.0.payload().len()
    }

    /// Get the instance ID of the sender.
    pub fn instance_id(&self) -> &str {
        self.0.instance_id()
    }

    /// Get the core node of the sender.
    pub fn core_node(&self) -> &str {
        self.0.core_node()
    }

    /// Producer's bound link_id, parsed from the inbound topic keyexpr.
    /// Returns an empty string for messages that arrived via a non-topic
    /// path (e.g. service responses), where no link_id is encoded in the
    /// reply keyexpr's caller slots.
    pub fn link_id(&self) -> &str {
        self.0.link_id()
    }

    /// Producer-stamped send time in nanoseconds since the Unix epoch, when the
    /// transport carried one (Zenoh source timestamp with timestamping enabled).
    /// `None` for the mock adapter, service-reply paths, or when timestamping is
    /// disabled. Used by `peppy stack benchmark` to compute delivery latency.
    pub fn source_timestamp_nanos(&self) -> Option<u64> {
        self.0.source_timestamp_nanos()
    }
}

impl From<pmi::TopicMessage> for Message {
    fn from(msg: pmi::TopicMessage) -> Self {
        Self(msg)
    }
}

/// Error returned by non-blocking receive operations when no message is
/// available or the channel has been closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TryRecvError {
    /// The channel is currently empty; no message is available.
    Empty,
    /// The channel has been closed and will not produce further messages.
    Disconnected,
}

impl std::fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TryRecvError::Empty => write!(f, "channel is empty"),
            TryRecvError::Disconnected => write!(f, "channel is disconnected"),
        }
    }
}

impl std::error::Error for TryRecvError {}

impl From<flume::TryRecvError> for TryRecvError {
    fn from(err: flume::TryRecvError) -> Self {
        match err {
            flume::TryRecvError::Empty => TryRecvError::Empty,
            flume::TryRecvError::Disconnected => TryRecvError::Disconnected,
        }
    }
}
