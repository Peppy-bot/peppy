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
    #[from]
    PeppyMessagingInterface(pmi::PeppyMessagingInterfaceError),

    // -- config-internal
    PeppyConfig(config::ConfigError),

    // -- auth: transport/HTTP failures (unreachable backend, unexpected status)
    Http(String),
    // -- auth: OAuth / identity failures with a user-actionable message
    Auth(String),
    // -- auth: no usable credential and not on an interactive terminal
    NotAuthenticated,

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
            Error::Http(msg) => write!(fmt, "{msg}"),
            Error::Auth(msg) => write!(fmt, "{msg}"),
            Error::NotAuthenticated => write!(
                fmt,
                "Not authenticated. Run `peppy login` or set PEPPY_API_KEY."
            ),
            Error::Peppy(e) => write!(fmt, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_failed_unescapes_newlines() {
        // A payload carrying the two characters `\` and `n` must render as a
        // real line break, since callers embed escaped newlines in messages.
        let err = Error::ExecutionFailed("line1\\nline2".to_string());
        assert_eq!(err.to_string(), "line1\nline2");
    }

    #[test]
    fn execution_failed_passes_plain_text_through() {
        let err = Error::ExecutionFailed("just text".to_string());
        assert_eq!(err.to_string(), "just text");
    }

    #[test]
    fn variant_prefixes_are_stable() {
        assert_eq!(
            Error::Io(std::io::Error::other("boom")).to_string(),
            "IO error: boom"
        );
        assert_eq!(Error::UnsupportedEngine.to_string(), "Unsupported engine");
        assert_eq!(
            Error::MissingEngineConfig.to_string(),
            "Missing engine config"
        );
        assert_eq!(
            Error::MissingMessagingRouter.to_string(),
            "Missing messaging router"
        );
        assert_eq!(
            Error::Zenohd("x".to_string()).to_string(),
            "Zenohd error: x"
        );
        assert_eq!(Error::Sync("x".to_string()).to_string(), "Sync error: x");
        assert_eq!(
            Error::InvalidNodeName("bad name".to_string()).to_string(),
            "Invalid node name: bad name"
        );
        assert_eq!(
            Error::NodeWatcher("x".to_string()).to_string(),
            "Node watcher error: x"
        );
    }
}
