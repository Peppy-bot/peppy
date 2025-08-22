use derive_more::From;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, From)]
pub enum Error {
    // -- general
    #[from]
    Io(std::io::Error),

    // -- serve
    UnsupportedEngine,

    // -- commands
    ExecutionFailed(String),
    PixiError(String),
    SyncError(String),

    // -- Node
    RootConfigurationNotFound,
    UnsupportedLanguage,
    FolderAlreadyExist(String),
    InvalidNodeName(String),
    GitConfigCreation(String),
    PeppyConfigCreation(String),
    PixiConfigCreation(String),
    RustConfigCreation(String),
    PythonConfigCreation(String),

    // -- messaging
    ConnectionError,
    PublishError {
        topic: String,
    },
    SubscribeError {
        topic: String,
    },
    ShutdownError,
    BackendError(String),

    // -- libs
    AskamaError(String),
}

impl core::fmt::Display for Error {
    fn fmt(&self, fmt: &mut core::fmt::Formatter) -> core::result::Result<(), core::fmt::Error> {
        write!(fmt, "{self:?}")
    }
}

impl std::error::Error for Error {}
