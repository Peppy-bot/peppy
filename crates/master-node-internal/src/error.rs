use peppylib::PeppyError;
use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    // -- general
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    PeppyMessagingInterface(#[from] pmi::PeppyMessagingInterfaceError),

    #[error(transparent)]
    Peppylib(#[from] PeppyError),

    #[error("task join failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}
