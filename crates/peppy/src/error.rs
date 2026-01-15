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
        match self {
            Error::Io(e) => write!(fmt, "IO error: {e}"),
            Error::UnsupportedEngine => write!(fmt, "Unsupported engine"),
            Error::MissingEngineConfig => write!(fmt, "Missing engine config"),
            Error::MissingMessagingRouter => write!(fmt, "Missing messaging router"),
            Error::ExecutionFailed(msg) => {
                // Convert escaped newlines to actual newlines for readable output
                let readable_msg = msg.replace("\\n", "\n");
                write!(fmt, "{readable_msg}")
            }
            Error::Zenohd(msg) => write!(fmt, "Zenohd error: {msg}"),
            Error::Sync(msg) => write!(fmt, "Sync error: {msg}"),
            Error::InvalidNodeName(name) => write!(fmt, "Invalid node name: {name}"),
            Error::NodeWatcher(msg) => write!(fmt, "Node watcher error: {msg}"),
            Error::PeppyMessagingInterface(e) => write!(fmt, "Messaging interface error: {e}"),
            Error::PeppyConfig(e) => write!(fmt, "Config error: {e}"),
            Error::Peppy(e) => write!(fmt, "{e}"),
        }
    }
}

impl std::error::Error for Error {}
