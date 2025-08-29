
pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    ConnectionError,
    ConfigurationError(String),
    PublishError { topic: String },
    SubscribeError { topic: String },
    ShutdownError,
    BackendError(String),
    MessagingSessionError(String),
    PublisherCreationError(String),
    MatchingListenerError(String),
    UnsupportedEngine,
    ZenohdError(String),

    // -- libs
    AskamaError(String),
}

impl core::fmt::Display for Error {
    fn fmt(&self, fmt: &mut core::fmt::Formatter) -> core::result::Result<(), core::fmt::Error> {
        write!(fmt, "{self:?}")
    }
}

impl std::error::Error for Error {}
