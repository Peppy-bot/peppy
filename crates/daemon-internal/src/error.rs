pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    // -- general daemon failures carrying a preformatted message
    #[error("{0}")]
    ExecutionFailed(String),

    // -- serve: a core node was requested without a messaging router
    #[error("Missing messaging router")]
    MissingMessagingRouter,

    // -- peppy-messaging-interface
    #[error("Messaging interface error: {0}")]
    PeppyMessagingInterface(#[from] pmi::PeppyMessagingInterfaceError),
}
