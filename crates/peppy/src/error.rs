use core::fmt::{Display, Formatter};
use derive_more::From;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, From)]
pub enum Error {
    // -- general
    #[from]
    Io(std::io::Error),

    // -- serve
    UnsupportedEngine,
    MissingEngineConfig,

    // -- commands
    ExecutionFailed(String),
    PixiError(String),
    ZenohdError(String),
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

    // -- pmi-internal
    PeppyMessagingInterfaceError(pmi::PeppyMessagingInterfaceError),

    // -- config-internal
    PeppyConfigError(config::ConfigError),

    // -- libs
    AskamaError(String),
}

impl Display for Error {
    fn fmt(&self, fmt: &mut Formatter) -> core::result::Result<(), core::fmt::Error> {
        write!(fmt, "{self:?}")
    }
}

impl std::error::Error for Error {}
