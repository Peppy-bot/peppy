pub use core_node_api::Payload;

/// A wrapper around `pmi::TopicMessage` to abstract away the underlying message implementation.
pub struct Message(pub(crate) pmi::TopicMessage);

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Message")
            .field("key_expr", &self.key_expr())
            .field("instance_id", &self.instance_id())
            .field("core_node", &self.core_node())
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

    /// Get the core node of the sender.
    pub fn core_node(&self) -> &str {
        self.0.core_node()
    }

    /// Get the variant of the sender (`"default"` baseline).
    pub fn variant(&self) -> &str {
        self.0.variant()
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
