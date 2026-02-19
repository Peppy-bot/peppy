use bytes::Bytes;

/// A wrapper around `bytes::Bytes` to avoid exposing the `bytes` crate in the public API.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Payload(pub(crate) Bytes);

impl Payload {
    /// Create a new `Payload` from a static slice.
    pub fn from_static(bytes: &'static [u8]) -> Self {
        Self(Bytes::from_static(bytes))
    }

    /// Create an empty `Payload`.
    pub fn new() -> Self {
        Self(Bytes::new())
    }

    /// Convert into the inner `Bytes`.
    pub(crate) fn into_inner(self) -> Bytes {
        self.0
    }

    /// Return a clone of the inner `Bytes`.
    pub(crate) fn to_bytes(&self) -> Bytes {
        self.0.clone()
    }
}

impl Default for Payload {
    fn default() -> Self {
        Self::new()
    }
}

impl From<Bytes> for Payload {
    fn from(bytes: Bytes) -> Self {
        Self(bytes)
    }
}

impl From<Vec<u8>> for Payload {
    fn from(vec: Vec<u8>) -> Self {
        Self(Bytes::from(vec))
    }
}

impl From<&'static [u8]> for Payload {
    fn from(slice: &'static [u8]) -> Self {
        Self(Bytes::from_static(slice))
    }
}

impl From<String> for Payload {
    fn from(s: String) -> Self {
        Self(Bytes::from(s))
    }
}

impl From<&'static str> for Payload {
    fn from(s: &'static str) -> Self {
        Self(Bytes::from_static(s.as_bytes()))
    }
}

impl AsRef<[u8]> for Payload {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl std::ops::Deref for Payload {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

impl PartialEq<Bytes> for Payload {
    fn eq(&self, other: &Bytes) -> bool {
        self.0 == other
    }
}

impl PartialEq<Payload> for Bytes {
    fn eq(&self, other: &Payload) -> bool {
        self == &other.0
    }
}

impl PartialEq<&[u8]> for Payload {
    fn eq(&self, other: &&[u8]) -> bool {
        self.0.as_ref() == *other
    }
}

impl PartialEq<Payload> for &[u8] {
    fn eq(&self, other: &Payload) -> bool {
        *self == other.0.as_ref()
    }
}

impl PartialEq<Vec<u8>> for Payload {
    fn eq(&self, other: &Vec<u8>) -> bool {
        self.0.as_ref() == other.as_slice()
    }
}

impl PartialEq<&Payload> for Payload {
    fn eq(&self, other: &&Payload) -> bool {
        self.0 == other.0
    }
}

impl PartialEq<Payload> for &Payload {
    fn eq(&self, other: &Payload) -> bool {
        self.0 == other.0
    }
}

/// A wrapper around `pmi::TopicMessage` to abstract away the underlying message implementation.
pub struct Message(pub(crate) pmi::TopicMessage);

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Message")
            .field("key_expr", &self.key_expr())
            .field("instance_id", &self.instance_id())
            .field("daemon_node", &self.daemon_node())
            .field("payload", &self.payload())
            .finish()
    }
}

impl Message {
    /// Get the payload of the message.
    pub fn payload(&self) -> Payload {
        Payload::from(self.0.payload().to_bytes())
    }

    /// Get the key expression of the message.
    pub fn key_expr(&self) -> &str {
        self.0.key_expr()
    }

    /// Get the instance ID of the sender.
    pub fn instance_id(&self) -> &str {
        self.0.instance_id()
    }

    /// Get the daemon node of the sender.
    pub fn daemon_node(&self) -> &str {
        self.0.daemon_node()
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

impl From<tokio::sync::mpsc::error::TryRecvError> for TryRecvError {
    fn from(err: tokio::sync::mpsc::error::TryRecvError) -> Self {
        match err {
            tokio::sync::mpsc::error::TryRecvError::Empty => TryRecvError::Empty,
            tokio::sync::mpsc::error::TryRecvError::Disconnected => TryRecvError::Disconnected,
        }
    }
}
