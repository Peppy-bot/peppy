use core::fmt::{Display, Formatter};
use derive_more::From;

pub type Result<T> = core::result::Result<T, Error>;

/// The exit code of a command that a Ctrl-C ended before the end of its
/// goal was known, as a shell gives a process that SIGINT ends.
pub const INTERRUPTED_EXIT_CODE: i32 = 130;

/// How a goal or a service call ended when it gave no result. Each end
/// exits with a code of its own, so a script reads the end from the exit
/// status alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallEnd {
    /// The provider refused the goal, or answered the request with an error
    /// reply.
    Refused,
    /// The goal ended failed.
    Failed,
    /// The goal ended cancelled, by the command's own cancel at a Ctrl-C or
    /// by another cause.
    Cancelled,
    /// The provider dropped the goal before it ended.
    Abandoned,
    /// The goal ended, and its end expired before the command read it.
    Expired,
}

impl CallEnd {
    /// The exit code of a command whose goal or call ended this way: 0 is a
    /// response or a completed goal, 1 an error of the command and 2 a
    /// command line that does not parse, so the ends start at 3.
    pub const fn exit_code(self) -> i32 {
        match self {
            CallEnd::Refused => 3,
            CallEnd::Failed => 4,
            CallEnd::Cancelled => 5,
            CallEnd::Abandoned => 6,
            CallEnd::Expired => 7,
        }
    }
}

#[derive(Debug, From)]
pub enum Error {
    // -- general
    #[from]
    Io(std::io::Error),

    // -- daemon: errors surfaced by the `daemon` crate (`peppy service serve`)
    #[from]
    Daemon(daemon::DaemonError),

    // -- commands
    ExecutionFailed(String),
    /// The goal or the service call of the command ended with no result;
    /// the text says how. The process ends with the code of `end`.
    Ended {
        end: CallEnd,
        message: String,
    },
    /// The operator pressed Ctrl-C again while the command waited for the
    /// end of a goal it had cancelled, so the end is not known. The process
    /// ends with code 130.
    Interrupted(String),
    Sync(String),

    // -- Node
    InvalidNodeName(String),

    // -- NodeWatcher
    NodeWatcher(String),

    // -- peppy-messaging-interface
    #[from]
    PeppyMessagingInterface(pmi::PeppyMessagingInterfaceError),

    // -- config: shared document model (node configs, runtime configs)
    PeppyConfig(config::ConfigError),
    // -- config: daemon-side documents (launcher files, peppy_config.json5)
    DaemonConfig(daemon_config::DaemonConfigError),

    // -- auth: CLI-side OAuth / identity failures with a user-actionable message
    Auth(String),
    // -- auth: errors surfaced by the `auth` engine crate
    #[from]
    AuthEngine(auth::AuthError),

    // -- peppylib
    #[from]
    Peppy(peppylib::PeppyError),
}

impl Display for Error {
    fn fmt(&self, fmt: &mut Formatter) -> core::result::Result<(), core::fmt::Error> {
        match self {
            Error::Io(e) => write!(fmt, "IO error: {e}"),
            Error::Daemon(e) => write!(fmt, "{e}"),
            Error::ExecutionFailed(msg) => {
                // Convert escaped newlines to actual newlines for readable output
                let readable_msg = msg.replace("\\n", "\n");
                write!(fmt, "{readable_msg}")
            }
            Error::Ended { message, .. } => write!(fmt, "{message}"),
            Error::Interrupted(msg) => write!(fmt, "{msg}"),
            Error::Sync(msg) => write!(fmt, "Sync error: {msg}"),
            Error::InvalidNodeName(name) => write!(fmt, "Invalid node name: {name}"),
            Error::NodeWatcher(msg) => write!(fmt, "Node watcher error: {msg}"),
            Error::PeppyMessagingInterface(e) => write!(fmt, "Messaging interface error: {e}"),
            Error::PeppyConfig(e) => write!(fmt, "Config error: {e}"),
            Error::DaemonConfig(e) => write!(fmt, "Config error: {e}"),
            Error::Auth(msg) => write!(fmt, "{msg}"),
            Error::AuthEngine(e) => write!(fmt, "{e}"),
            Error::Peppy(e) => write!(fmt, "{e}"),
        }
    }
}

impl Error {
    /// The error of a goal or a service call that ended as `end`.
    pub fn ended(end: CallEnd, message: impl Into<String>) -> Self {
        Error::Ended {
            end,
            message: message.into(),
        }
    }

    /// The exit code of a command that ends with this error: the code of
    /// the end of its goal or call, [`INTERRUPTED_EXIT_CODE`] after a
    /// second Ctrl-C, and 1 for every other error of the command.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Ended { end, .. } => end.exit_code(),
            Error::Interrupted(_) => INTERRUPTED_EXIT_CODE,
            _ => 1,
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

    /// Every end of a goal or a call has an exit code of its own, apart from
    /// success (0), an error of the command (1), a command line that does
    /// not parse (2) and a second Ctrl-C (130).
    #[test]
    fn each_end_exits_with_a_code_of_its_own() {
        let ends = [
            CallEnd::Refused,
            CallEnd::Failed,
            CallEnd::Cancelled,
            CallEnd::Abandoned,
            CallEnd::Expired,
        ];
        let mut codes: Vec<i32> = ends
            .iter()
            .map(|&end| Error::ended(end, "how it ended").exit_code())
            .collect();
        codes.extend([
            0,
            Error::ExecutionFailed("x".to_owned()).exit_code(),
            2,
            Error::Interrupted("x".to_owned()).exit_code(),
        ]);
        let distinct: std::collections::BTreeSet<i32> = codes.iter().copied().collect();
        assert_eq!(distinct.len(), codes.len(), "{codes:?}");
        assert_eq!(Error::ExecutionFailed("x".to_owned()).exit_code(), 1);
        assert_eq!(
            Error::Interrupted("x".to_owned()).exit_code(),
            INTERRUPTED_EXIT_CODE
        );
    }

    #[test]
    fn an_ended_call_displays_its_message() {
        assert_eq!(
            Error::ended(CallEnd::Refused, "Launch refused (busy): later").to_string(),
            "Launch refused (busy): later"
        );
    }

    #[test]
    fn variant_prefixes_are_stable() {
        assert_eq!(
            Error::Io(std::io::Error::other("boom")).to_string(),
            "IO error: boom"
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
