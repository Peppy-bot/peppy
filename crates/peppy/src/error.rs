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
    MissingMessagingRouter,

    // -- commands
    ExecutionFailed(String),
    Zenohd(String),
    Sync(String),

    // -- Node
    InvalidNodeName(String),

    // -- NodeWatcher
    NodeWatcher(String),

    // -- pmi-internal
    PeppyMessagingInterface(pmi::PeppyMessagingInterfaceError),

    // -- config-internal
    PeppyConfig(config::ConfigError),

    // -- peppylib
    #[from]
    Peppy(peppylib::PeppyError),
}

impl Display for Error {
    fn fmt(&self, fmt: &mut Formatter) -> core::result::Result<(), core::fmt::Error> {
        write!(fmt, "{self:?}")
    }
}

impl std::error::Error for Error {}
