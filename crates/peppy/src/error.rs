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
    Zenohd(String),
    Sync(String),

    // -- Node
    UnsupportedLanguage,
    FolderAlreadyExist(String),
    InvalidNodeName(String),
    GitConfigCreation(String),
    PeppyConfigCreation(String),
    RustConfigCreation(String),
    PythonConfigCreation(String),

    // -- NodeWatcher
    NodeWatcher(String),

    // -- pmi-internal
    PeppyMessagingInterface(pmi::PeppyMessagingInterfaceError),

    // -- generator-internal
    #[from]
    GeneratorError(generator::GeneratorError),

    // -- node-stack-internal
    #[from]
    NodeStackError(node_stack::NodeStackError),

    // -- config-internal
    PeppyConfig(config::ConfigError),

    // -- libs
    Askama(String),
}

impl Display for Error {
    fn fmt(&self, fmt: &mut Formatter) -> core::result::Result<(), core::fmt::Error> {
        write!(fmt, "{self:?}")
    }
}

impl std::error::Error for Error {}
