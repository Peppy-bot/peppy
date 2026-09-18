//! What can go wrong building a chain. Each variant names the thing the caller
//! got wrong, so a message is actionable without reading this crate.

/// A chain could not be built from the URDF and spec given.
#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    #[error("URDF has no link named '{0}'")]
    NoSuchLink(String),
    #[error("tip link '{tip}' is not below base link '{base}'")]
    TipNotBelowBase { tip: String, base: String },
    #[error("joint '{0}' is not on the path from the base to the tip")]
    JointNotOnPath(String),
    #[error("joint '{0}' is fixed, so it cannot be actuated")]
    JointDoesNotMove(String),
    #[error("joint '{0}' is named twice; each entry of `q` drives one joint")]
    DuplicateJoint(String),
    #[error(
        "joint '{follower}' mimics '{leader}', and this chain actuates one of them; \
         the coupled joint would be left behind"
    )]
    MimicCoupling { follower: String, leader: String },
    #[error("joint '{joint}' has unusable limits [{lo}, {hi}]")]
    UnusableLimit { joint: String, lo: f64, hi: f64 },
    #[error("'{tool}' is not a link fixed below the tip '{tip}'")]
    ToolNotFixedBelowTip { tool: String, tip: String },
    #[error("chain has {found} actuated joints, expected {expected}")]
    JointCount { expected: usize, found: usize },
    #[error("{0}")]
    Urdf(String),
}

impl From<String> for ChainError {
    fn from(message: String) -> Self {
        ChainError::Urdf(message)
    }
}
