use derive_more::From;

use crate::wire::{SegmentError, SenderTargetError};

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
}

impl core::fmt::Display for Error {
    fn fmt(&self, fmt: &mut core::fmt::Formatter) -> core::result::Result<(), core::fmt::Error> {
        write!(fmt, "{self:?}")
    }
}

impl std::error::Error for Error {}
