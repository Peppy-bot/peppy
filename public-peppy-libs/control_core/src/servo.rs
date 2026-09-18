//! Convergence tolerances for a Cartesian servo loop: how close to a pose
//! target counts as arrived. Manipulator-agnostic, in SI, and deliberately
//! defaults rather than law, so a caller with a tighter task can pass its own.

/// Position slack (m) inside which a pose goal counts as reached: MoveIt
/// Servo's `pose_tracking.linear_tolerance` default. Well above the round-trip
/// noise of a forward-kinematics evaluation, and below what an operator can
/// see.
pub const POSITION_TOLERANCE_M: f64 = 1e-3;

/// Orientation slack (rad) for the same judgement: MoveIt Servo's
/// `pose_tracking.angular_tolerance` default, about 0.57 degrees.
pub const ORIENTATION_TOLERANCE_RAD: f64 = 1e-2;
