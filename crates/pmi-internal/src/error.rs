use derive_more::From;

use crate::wire::{IfaceError, SegmentError};

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
    InvalidIface(IfaceError),
}

impl core::fmt::Display for Error {
    fn fmt(&self, fmt: &mut core::fmt::Formatter) -> core::result::Result<(), core::fmt::Error> {
        write!(fmt, "{self:?}")
    }
}

impl std::error::Error for Error {}
