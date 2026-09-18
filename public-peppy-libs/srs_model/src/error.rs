//! The crate's error type.

/// A failure building an SRS arm / FK model from a URDF.
/// [`UrdfRead`](Self::UrdfRead) is a missing or unreadable file;
/// [`NotSrsArm`](Self::NotSrsArm) is a chain that parsed but is not a clean 7-DOF
/// SRS arm; other opaque parse/model failures use the [`Model`](Self::Model)
/// catch-all.
#[derive(Debug, thiserror::Error)]
pub enum SrsError {
    /// The URDF file could not be read from disk.
    #[error("read URDF '{path}': {source}")]
    UrdfRead {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// The chain from the base link is not a clean 7-DOF SRS arm: the wrong
    /// revolute-joint count, a non-revolute movable joint, or an ambiguous fork.
    #[error("{0}")]
    NotSrsArm(String),

    /// The URDF parsed but is otherwise unusable (parse failure, a missing link,
    /// or non-SRS geometry). Carries the underlying reason.
    #[error("{0}")]
    Model(String),

    /// A tool link mounted via [`Arm::with_tool_link`](crate::Arm::with_tool_link) is not a rigid
    /// transform: a non-finite component, or a rotation that is not unit length.
    #[error("tool transform: {0}")]
    Tool(String),
}

/// Lower-level helpers report opaque string reasons; fold them into
/// [`Model`](SrsError::Model) so `?` propagates them.
impl From<String> for SrsError {
    fn from(reason: String) -> Self {
        SrsError::Model(reason)
    }
}

impl From<&str> for SrsError {
    fn from(reason: &str) -> Self {
        SrsError::Model(reason.to_string())
    }
}
