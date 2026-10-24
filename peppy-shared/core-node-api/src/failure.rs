//! The failure kinds of the daemon's own protocol: the cause of a refused or
//! failed daemon goal, and of the error reply of a daemon service.
//!
//! The daemon gives a kind from a closed set: the kinds of peppy
//! ([`PeppyFailureKind`]) and the kind of its own, [`DaemonFailureKind::Busy`],
//! as a contract adds kinds of its own to the kinds of peppy. The daemon and
//! each of its clients share this set. A client reads the kind of a daemon
//! reply into it once ([`DaemonFailure::parse`]): a kind outside the set is
//! an error, never a failure of another kind.

use config::failure_kind::{FailureKindName, PeppyFailureKind};
use std::fmt;

/// A failure kind that the daemon gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DaemonFailureKind {
    /// A kind of peppy.
    Peppy(PeppyFailureKind),
    /// Other work holds the stack of the daemon: a stack change, a node
    /// goal, or a launch that reserved the daemon. The same request can
    /// succeed once that work ends.
    Busy,
}

impl DaemonFailureKind {
    /// Every kind of the daemon: the kinds of peppy, then `busy`.
    pub const ALL: [Self; 6] = [
        Self::Peppy(PeppyFailureKind::AttemptFailed),
        Self::Peppy(PeppyFailureKind::Timeout),
        Self::Peppy(PeppyFailureKind::PreconditionFailed),
        Self::Peppy(PeppyFailureKind::ResourceUnavailable),
        Self::Peppy(PeppyFailureKind::Rejected),
        Self::Busy,
    ];

    /// The name of the kind on the wire and in documents.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Peppy(kind) => kind.name(),
            Self::Busy => "busy",
        }
    }

    /// What the kind means.
    pub const fn meaning(self) -> &'static str {
        match self {
            Self::Peppy(kind) => kind.meaning(),
            Self::Busy => {
                "Other work holds the stack of the daemon; the same request can succeed once \
                 that work ends"
            }
        }
    }

    /// The kind of the daemon that `name` names, or `None` when the daemon
    /// gives no such kind.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }
}

impl From<PeppyFailureKind> for DaemonFailureKind {
    fn from(kind: PeppyFailureKind) -> Self {
        Self::Peppy(kind)
    }
}

impl From<DaemonFailureKind> for FailureKindName {
    fn from(kind: DaemonFailureKind) -> Self {
        match kind {
            DaemonFailureKind::Peppy(kind) => kind.into(),
            DaemonFailureKind::Busy => FailureKindName::parse(kind.name())
                .expect("each kind of the daemon is a valid failure kind name"),
        }
    }
}

impl fmt::Display for DaemonFailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why the daemon refused or failed a goal, or gave an error reply: a kind of
/// the daemon and a message for a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonFailure {
    pub kind: DaemonFailureKind,
    pub message: String,
}

impl DaemonFailure {
    pub fn new(kind: impl Into<DaemonFailureKind>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
        }
    }

    /// Parses the failure of a daemon reply: its kind, as the wire names it,
    /// and its message. A kind that the daemon does not give is an error
    /// that keeps the message.
    pub fn parse(kind: &str, message: &str) -> Result<Self, UnknownDaemonFailureKind> {
        let Some(parsed) = DaemonFailureKind::parse(kind) else {
            return Err(UnknownDaemonFailureKind {
                kind: kind.to_owned(),
                message: message.to_owned(),
            });
        };
        Ok(Self::new(parsed, message))
    }
}

/// `kind: message`, as a failure on the wire reads.
impl fmt::Display for DaemonFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

/// A daemon reply whose failure kind is not a kind of the daemon, with the
/// message of that failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "the daemon gave the failure kind `{kind}`, which is not a kind of the daemon (message: \
     {message:?})"
)]
pub struct UnknownDaemonFailureKind {
    pub kind: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_of_the_daemon_has_a_valid_name_that_parses_back_to_it() {
        for kind in DaemonFailureKind::ALL {
            let name = FailureKindName::from(kind);
            assert_eq!(name.as_str(), kind.name());
            assert_eq!(DaemonFailureKind::parse(kind.name()), Some(kind));
            assert!(!kind.meaning().is_empty());
        }
    }

    #[test]
    fn the_daemon_gives_every_kind_of_peppy_and_busy() {
        for kind in PeppyFailureKind::ALL {
            assert!(DaemonFailureKind::ALL.contains(&DaemonFailureKind::from(kind)));
        }
        assert_eq!(
            DaemonFailureKind::parse("busy"),
            Some(DaemonFailureKind::Busy)
        );
        assert_eq!(
            PeppyFailureKind::from_name("busy"),
            None,
            "busy is a kind of the daemon, not of peppy"
        );
    }

    #[test]
    fn a_failure_parses_with_a_kind_of_the_daemon() {
        assert_eq!(
            DaemonFailure::parse("busy", "a stack change runs"),
            Ok(DaemonFailure::new(
                DaemonFailureKind::Busy,
                "a stack change runs"
            ))
        );
        assert_eq!(
            DaemonFailure::parse("precondition_failed", "copy `alpha` already exists"),
            Ok(DaemonFailure::new(
                PeppyFailureKind::PreconditionFailed,
                "copy `alpha` already exists"
            ))
        );
    }

    #[test]
    fn a_kind_outside_the_daemon_is_an_error_that_keeps_the_message() {
        let error = DaemonFailure::parse("unreachable", "no plan").unwrap_err();
        assert_eq!(
            error,
            UnknownDaemonFailureKind {
                kind: "unreachable".to_owned(),
                message: "no plan".to_owned(),
            }
        );
        assert_eq!(
            error.to_string(),
            "the daemon gave the failure kind `unreachable`, which is not a kind of the daemon \
             (message: \"no plan\")"
        );
    }
}
