//! Kinematics for a serial chain read from a URDF: forward kinematics, the
//! geometric Jacobian, and damped resolved-rate inverse kinematics.
//!
//! Nothing here assumes a topology. A [`Chain`] is any path through any URDF
//! with `N` actuated joints - a seven-axis arm, a five-axis arm, a leg, a
//! pan-tilt head - and every type is generic over `N`, fixed at compile time so
//! a control tick allocates nothing.
//!
//! Build one from a parsed URDF and a [`ChainSpec`] naming where the chain
//! starts, where it ends, and which joints are yours to move:
//!
//! ```no_run
//! # use chain_kinematics::{Chain, ChainSpec, JointSelection};
//! let robot = urdf_rs::read_from_string(MY_URDF)?;
//! let chain = Chain::<5>::from_urdf(&robot, &ChainSpec {
//!     base_link: None,
//!     tip_link: "gripper_frame_link",
//!     joints: JointSelection::Named(&[
//!         "shoulder_pan", "shoulder_lift", "elbow_flex", "wrist_flex", "wrist_roll",
//!     ]),
//! })?;
//! let ee = chain.at(&[0.0; 5]).ee_pose();
//! # const MY_URDF: &str = "";
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! The tip and the joint order are **named, not discovered**. Discovering a tip
//! by counting joints only works for the arm it was written for, and inferring
//! joint order from the URDF's own ordering is luck rather than a contract: the
//! order is the order of `q`, and it is usually the order the robot's wire
//! protocol uses, which no URDF knows about.
//!
//! Frames: forward kinematics reports poses in the **base frame** named by the
//! spec, and [`Chain::base_from_world`] relates that to the URDF root for a
//! caller that needs the world frame (gravity, or composing two chains of one
//! robot).
#![forbid(unsafe_code)]

mod chain;
mod dynamics;
mod error;
mod ik;
mod jacobian;
mod payload;
mod rate;
mod servo;
mod tree;

pub use chain::{Chain, ChainSpec, JointSelection, Posed};
pub use error::ChainError;
pub use ik::{
    ACCEPT_ORIENTATION_RAD, ACCEPT_POSITION_M, Goal, Kinematics, SEED_TABLE_SIZE, SolveError,
};
pub use jacobian::{
    Jacobian, JacobianPinv, damped_pseudo_inverse, manipulability, null_space_projector,
    try_pseudo_inverse,
};
pub use payload::{Payload, segments_carrying};
pub use rate::{DEFAULT_DLS_LAMBDA, rate_step};
pub use servo::{
    EeCaps, NoSmoothing, ServoLimits, ServoState, ServoStep, ServoTolerances, Smoother,
    TRACKING_FLOOR_M, ToleranceError, interpolate, rate_step_toward, rollout,
};
pub use tree::{JointKind, Tree};

/// Re-exported so downstream crates speak the same `nalgebra` version this crate
/// was built against, rather than each picking their own and finding the
/// `Isometry3`s do not match.
pub use nalgebra;

/// Inclusive joint position limit, radians (or metres for a prismatic joint).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limit {
    pub lo: f64,
    pub hi: f64,
}

impl Limit {
    /// True if `x` lies within `[lo, hi]`. Non-finite `x` (NaN/inf) compares
    /// false on both sides, so it is rejected.
    pub fn contains(&self, x: f64) -> bool {
        self.lo <= x && x <= self.hi
    }

    /// `x` brought inside the limit.
    pub fn clamp(&self, x: f64) -> f64 {
        x.clamp(self.lo, self.hi)
    }

    /// The centre of the range. The seed of last resort for a caller with no
    /// better one: an all-zeros configuration sits exactly on a limit for any
    /// joint whose range does not straddle zero.
    pub fn midpoint(&self) -> f64 {
        (self.lo + self.hi) / 2.0
    }
}
