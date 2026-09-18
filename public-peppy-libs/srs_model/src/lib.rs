//! Kinematics + dynamics for a 7-DOF SRS (spherical-revolute-spherical) arm.
//!
//! Build an [`Arm`] from a URDF once; everything hangs off it:
//!
//! - [`Arm::at`] poses the arm and returns a [`Posed`] view for forward
//!   kinematics ([`Posed::ee_pose`]) and feedforward dynamics
//!   ([`Posed::gravity_torques`], [`Posed::coriolis_torques`]). The dynamics carry
//!   the distal payload (gripper, fingers, tools) lumped into the last segment, and
//!   are validated in tests against KDL `TreeIdSolver_RNE` reference values (tree
//!   inverse dynamics, so the branched gripper is included); gravity additionally
//!   against the potential-energy gradient.
//! - [`Arm::solve_ik`] is closed-form arm-angle (Shimizu) inverse kinematics
//!   ([`ArmAnglePolicy`], [`Solution`]); [`Arm::arm_angle`] reports a config's arm angle.
//! - [`Arm::limits`] are the URDF joint limits; [`Arm::base_pose`] /
//!   [`Arm::world_pose`] convert between world and arm base frames.
//!
//! Frames: FK and IK work in the **arm base frame** (the arm's own mounting link).
//! Gravity / Coriolis compute in the **world frame**, because gravity is a world
//! quantity (acts along world -Z). The fixed world <-> base mount transform
//! relating the two is captured at load (see [`Arm::base_from_world`]).
//!
//! Robot-agnostic: geometry, joint limits and inertials are all derived from
//! whatever URDF the caller passes, and a non-SRS chain is rejected with `Err`.
//! There is no per-robot configuration baked in: the caller supplies the URDF and
//! the `base_link` where the SRS chain starts; the wrist is found by walking 7
//! revolute joints out from it, and everything past the wrist (gripper, fingers,
//! tools) is carried as the distal payload in gravity / Coriolis. Left vs right is
//! a different chain in the same URDF, selected by the base link (the mirror is
//! re-derived from the URDF geometry, never sign-flipped). For gravity to point
//! the right way, the URDF must contain the kinematic tree from the world/body
//! root down to `base_link` (the mount), not just the bare arm chain.
//!
//! Hardware-free: it defines its own [`ARM_DOF`] rather than depending on any
//! hardware crate, so the full IK<->FK round-trip runs under a plain `cargo test`
//! on any host.
#![forbid(unsafe_code)]

mod arm;
mod error;
mod fk;
mod ik;
mod jacobian;
mod model;

/// The library entry point: build an [`Arm`] from a URDF, then read FK, gravity,
/// Coriolis, and IK off it. [`Posed`] is the read-only view returned by
/// [`Arm::at`]; [`ArmAnglePolicy`] / [`Solution`] are the IK types.
pub use arm::{Arm, DEFAULT_DLS_LAMBDA};
pub use error::SrsError;
pub use ik::{ArmAnglePolicy, Solution};

/// The arm posed at one configuration: the chain's own read-only view, at
/// seven joints. Every pose-dependent quantity, the feedforward dynamics
/// included, is read through it, so it can only be queried after a pose.
pub type Posed<'a> = chain_kinematics::Posed<'a, ARM_DOF>;

/// Differential kinematics: the geometric [`Jacobian`] (read off a [`Posed`] view
/// via [`Posed::jacobian`]) and its redundancy-aware inverses and helpers for
/// velocity-level control.
pub use jacobian::{
    Jacobian, JacobianPinv, damped_pseudo_inverse, manipulability, null_space_projector,
    try_pseudo_inverse,
};

/// Degrees of freedom of the arm. The URDF chain is validated against this at
/// load (the closed-form arm-angle redundancy resolution is specific to 7 DOF).
pub const ARM_DOF: usize = 7;

/// One joint-space configuration, j1..j7 in radians.
pub type JointVec = [f64; ARM_DOF];

/// Inclusive joint position limit, radians: the chain's own type, re-exported
/// rather than mirrored, so the limits [`Arm::limits`] reports and the limits
/// the IK checks against cannot be two structs that happen to agree.
pub use chain_kinematics::Limit;

/// Smallest sine of an angle between two unit axes (or two link directions) we
/// treat as non-degenerate (~1e-6 rad). Below it the perpendicular / cross
/// direction is ill-conditioned to normalize, so the caller bails out (parallel
/// axes, straight arm). Sites that test a `sin²` quantity compare its square.
pub(crate) const PARALLEL_SIN_EPS: f64 = 1e-6;

/// Re-export the linear-algebra types so downstream crates speak the same
/// `nalgebra` version this crate was built against, rather than each picking
/// their own and finding the `Isometry3`s do not match.
pub use nalgebra;

/// Re-export the chain this arm is built on, so a consumer holding an [`Arm`]
/// can reach the topology-agnostic laws ([`Arm::chain`] feeds them) without
/// naming a second version of the crate.
pub use chain_kinematics;

/// Test fixtures: load a concrete SRS arm (the OpenArm V1.0) from the bundled
/// fixture URDF, giving the agnostic tests a real arm to check against.
/// `side` is `"left"`/`"right"`; left vs right is encoded entirely by the link
/// names, so there is no side parameter beyond which links are selected.
#[cfg(test)]
pub(crate) mod test_support {
    use crate::fk::ForwardKinematics;
    use crate::model::ArmModel;

    /// A concrete SRS arm used only as a test fixture: the real OpenArm V1.0
    /// description, gripper fingers included, so the distal-payload path is
    /// exercised by simply loading it. Production callers pass their own URDF via
    /// the node configuration; this is wired only under `cfg(test)`.
    pub(crate) const FIXTURE_URDF: &str = include_str!("../tests/fixtures/openarm_v10.urdf");

    /// Base link where the fixture's 7-DOF chain for `side` starts.
    fn base(side: &str) -> String {
        format!("openarm_{side}_link0")
    }

    pub(crate) fn v1_fk(side: &str) -> ForwardKinematics {
        ForwardKinematics::from_urdf(FIXTURE_URDF, &base(side)).expect("load fixture fk")
    }

    pub(crate) fn v1_model(side: &str) -> ArmModel {
        ArmModel::from_fk(&mut v1_fk(side)).expect("load fixture model")
    }

    /// The fixture's joint limits, the solver's in-limit test (see `ik::solve`).
    pub(crate) fn v1_limits(side: &str) -> [crate::Limit; crate::ARM_DOF] {
        v1_fk(side).limits()
    }
}
