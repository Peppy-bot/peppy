//! Error types crossing the runtime's boundaries: building a server from a
//! bundle and composing servers into a set, feeding snapshots through the
//! topic policies, and reporting bridge failures back to MCP clients.

use std::fmt;

/// Why a bundle plus its registered handlers cannot become a server, or why
/// servers cannot compose into a set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    /// A handler was registered under a name no tool entry carries.
    UnknownToolHandler { name: String },
    /// A tool entry has no registered handler, so calls could never route.
    MissingToolHandler { name: String },
    /// A task handler was registered under a name no task entry carries.
    UnknownTaskHandler { name: String },
    /// A task entry has no registered handler, so calls could never route.
    MissingTaskHandler { name: String },
    /// A tool or task entry's derived input schema does not compile.
    InvalidInputSchema { name: String, error: String },
    /// Two catalog entries collide on a public name or URI.
    DuplicateName { name: String },
    /// A set was composed with no exposure to serve.
    NoExposures,
    /// Two servers in a set serve the same exposure identity, which is one
    /// endpoint path.
    DuplicateExposure { name: String, tag: String },
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownToolHandler { name } => {
                write!(
                    f,
                    "a handler is registered for `{name}`, which is not a tool in the bundle"
                )
            }
            Self::MissingToolHandler { name } => {
                write!(f, "tool `{name}` has no registered handler")
            }
            Self::UnknownTaskHandler { name } => {
                write!(
                    f,
                    "a task handler is registered for `{name}`, which is not a task in the bundle"
                )
            }
            Self::MissingTaskHandler { name } => {
                write!(f, "task `{name}` has no registered handler")
            }
            Self::InvalidInputSchema { name, error } => {
                write!(
                    f,
                    "catalog entry `{name}` input schema does not compile: {error}"
                )
            }
            Self::DuplicateName { name } => {
                write!(f, "catalog name `{name}` appears more than once")
            }
            Self::NoExposures => write!(f, "an exposure set must serve at least one exposure"),
            Self::DuplicateExposure { name, tag } => {
                write!(f, "exposure `{name}:{tag}` is listed more than once")
            }
        }
    }
}

impl std::error::Error for BuildError {}

/// Why a policy-gated snapshot publish was refused. The pump drops the
/// message, the previous snapshot stays current, and freshness decides when
/// readers start seeing the gap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishError {
    /// The snapshot is not a JSON object, so representation fields cannot
    /// resolve.
    NotAnObject,
    /// A representation field named by the exposure is absent or has the
    /// wrong JSON type.
    Field {
        role: &'static str,
        name: String,
        problem: String,
    },
    /// The frame's encoding label names a layout this runtime cannot decode.
    UnsupportedEncoding { encoding: String },
    /// The frame bytes do not decode under their declared encoding and
    /// dimensions.
    BadFrame { detail: String },
    /// The serialized snapshot exceeds `max_result_bytes` and the policy is
    /// to reject, or downscaling could not fit it.
    Oversize { size: u64, limit: u64 },
}

impl fmt::Display for PublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnObject => write!(f, "snapshot content is not a JSON object"),
            Self::Field {
                role,
                name,
                problem,
            } => {
                write!(f, "representation field `{name}` ({role}) {problem}")
            }
            Self::UnsupportedEncoding { encoding } => {
                write!(f, "cannot transcode frames with encoding `{encoding}`")
            }
            Self::BadFrame { detail } => write!(f, "frame does not decode: {detail}"),
            Self::Oversize { size, limit } => {
                write!(
                    f,
                    "serialized snapshot of {size} bytes exceeds the {limit} byte limit"
                )
            }
        }
    }
}

impl std::error::Error for PublishError {}

/// A bridge failure surfaced to the MCP client as a tool-level error. Tool
/// errors stay readable text; protocol errors are reserved for requests
/// that never reached a bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallError {
    /// The linked provider is unreachable.
    Unavailable(String),
    /// The provider did not answer within the exposure's deadline.
    Deadline(String),
    /// The provider answered with a failure.
    Failed(String),
}

impl fmt::Display for ToolCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(detail) => write!(f, "provider unavailable: {detail}"),
            Self::Deadline(detail) => write!(f, "deadline exceeded: {detail}"),
            Self::Failed(detail) => write!(f, "call failed: {detail}"),
        }
    }
}

impl std::error::Error for ToolCallError {}
