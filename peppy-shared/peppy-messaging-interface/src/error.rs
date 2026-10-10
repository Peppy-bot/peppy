use derive_more::From;

use crate::types::PublisherQoS;
use crate::wire::{SegmentError, SenderTargetError};
use config::node::TopicRetention;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, From)]
pub enum Error {
    #[from]
    Io(std::io::Error),

    ConfigurationError(String),
    PublishError {
        topic: String,
    },
    SubscribeError {
        topic: String,
    },
    ShutdownError,
    BackendError(String),
    MessagingSessionError(String),
    PublisherCreationError(String),
    UnsupportedEngine,
    ZenohdError(String),
    ZenohDConfigurationNotFound,
    #[from]
    InvalidSegment(SegmentError),
    #[from]
    InvalidSenderTarget(SenderTargetError),
    /// A pairing publish was built without the peer it is for. Every pairing
    /// publish is addressed to one peer of its slot.
    PairingPublishNamesNoPeer,
    /// Only a pairing publish names a peer: contract and node emissions are
    /// addressed to whoever subscribes.
    PeerOnNonPairingPublish,
    /// A pairing subscription says which recipient it stands for: its own
    /// slot, or any peer when it observes the pairing.
    PairingSubscriptionNamesNoRecipient,
    /// Only a pairing subscription names a recipient.
    RecipientOnNonPairingSubscription,
    /// A publish key takes one retention for the life of its session, and a
    /// retaining key one QoS; this publish asked for another.
    RetainingTopicMismatch {
        topic: String,
        declared_retention: TopicRetention,
        /// The QoS a retaining key took; a live-only key takes every QoS.
        declared_qos: Option<PublisherQoS>,
        requested_retention: TopicRetention,
        requested_qos: PublisherQoS,
    },
}

impl core::fmt::Display for Error {
    fn fmt(&self, fmt: &mut core::fmt::Formatter) -> core::result::Result<(), core::fmt::Error> {
        match self {
            Self::RetainingTopicMismatch {
                topic,
                declared_retention,
                declared_qos,
                requested_retention,
                requested_qos,
            } => {
                write!(
                    fmt,
                    "topic key `{topic}` is declared with retention `{declared_retention}`"
                )?;
                if let Some(declared_qos) = declared_qos {
                    write!(fmt, " and QoS {declared_qos:?}")?;
                }
                write!(
                    fmt,
                    ", and this publisher asks for retention `{requested_retention}` and QoS \
                     {requested_qos:?}; declare every publisher of the topic with the same retention"
                )?;
                if declared_qos.is_some() {
                    write!(fmt, " and QoS")?;
                }
                Ok(())
            }
            other => write!(fmt, "{other:?}"),
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    /// An operation that needs an open messaging session ran before
    /// `start_session` or after `stop_session`.
    pub(crate) fn session_not_initialized() -> Self {
        Self::MessagingSessionError("Session not initialized".to_string())
    }
}

/// Test support shared by this crate's unit tests.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{Error, Result};

    /// Asserts that `result` is the error an operation reports when it runs
    /// without an open messaging session.
    pub(crate) fn assert_session_not_initialized<T>(operation: &str, result: Result<T>) {
        match result {
            Err(Error::MessagingSessionError(message)) => {
                assert_eq!(message, "Session not initialized", "{operation}");
            }
            Err(other) => panic!("{operation}: expected the missing-session error, got {other:?}"),
            Ok(_) => panic!("{operation}: succeeded without a session"),
        }
    }
}
