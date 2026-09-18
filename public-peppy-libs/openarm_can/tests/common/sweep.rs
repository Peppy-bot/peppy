// Command sweep shared by the vcan fixture capture and the replay tests.
//
// Included by `protocol.rs` (hardware-free encode replay) and by
// `tests/vcan.rs` (bus-level replay), so it must stay dependency-free.
//
// The differential fixtures in `tests/fixtures/` were captured by driving
// the C++-backed FFI build of this crate (enactic/openarm_can 1.2.8)
// through exactly these values on vcan; the replay drives the native
// implementation through them and byte-diffs the frames.
//
// The arm fixture's j3/j4 rows are the one deliberate departure from that
// capture. Those joints report a VelocityMax of 10 rad/s on this hardware,
// which the reference decodes as 8, so every non-saturating dq encodes
// differently. Six lines carry our scale rather than the reference's; the
// reference is wrong for these motors, so matching it would mean shipping a
// 20% velocity error to keep a test green.

/// MIT command tuples `(kp, kd, q, dq, tau)` broadcast to all seven arm
/// joints. Covers the zero point, typical gains, full-scale, beyond-clamp,
/// negative full-scale, denormal-small, and truncation-sensitive values.
pub const ARM_MIT_SWEEP: &[(f64, f64, f64, f64, f64)] = &[
    (0.0, 0.0, 0.0, 0.0, 0.0),
    (50.0, 1.0, 0.5, 0.1, 0.2),
    (500.0, 5.0, 12.5, 45.0, 54.0),
    (501.0, 5.1, 13.0, 46.0, 55.0),
    (0.0, 0.0, -12.5, -45.0, -54.0),
    (0.001, 0.005, 1e-9, -1e-9, 0.0),
    (123.456, 2.345, 1.0, -3.21, 7.89),
    (250.0, 2.5, -7.7, 12.3, -33.3),
];

/// One arm command with distinct per-joint values, rows `[kp, kd, q, dq, tau]`.
pub const ARM_MIT_INDEXED: [[f64; 7]; 5] = [
    [10.0, 20.0, 30.0, 40.0, 50.0, 60.0, 70.0],
    [0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 3.5],
    [-3.0, -2.0, -1.0, 0.0, 1.0, 2.0, 3.0],
    [-1.1, -0.6, -0.1, 0.4, 0.9, 1.4, 1.9],
    [-9.0, -6.0, -3.0, 0.0, 3.0, 6.0, 9.0],
];

/// MIT command tuples `(kp, kd, q, dq, tau)` for the v1.0 gripper motor.
/// The second entry's q is the v1 open angle, which the platform defines as
/// the literal -1.0472 rather than -pi/3.
#[allow(clippy::approx_constant)]
pub const GRIPPER_MIT_SWEEP: &[(f64, f64, f64, f64, f64)] = &[
    (0.0, 0.0, 0.0, 0.0, 0.0),
    (100.0, 1.5, -1.0472, 0.0, 0.0),
    (500.0, 5.0, 12.5, 30.0, 10.0),
    (2.0, 0.1, 0.5, -0.5, 0.25),
];

/// POS_FORCE command tuples `(q_rad, speed_rad_s, torque_pu)` for the v2.0
/// gripper motor. Speeds beyond 100 rad/s and below zero exercise the clamp;
/// torque stays in `0..=1` because the implementations diverge outside it
/// (the C++ clamps, the native driver rejects), so out-of-range values can
/// never byte-compare.
pub const POS_FORCE_SWEEP: &[(f64, f64, f64)] = &[
    (0.0, 5.0, 0.5),
    (std::f64::consts::FRAC_PI_2, 5.0, 0.5),
    (0.5, 150.0, 1.0),
    (1.2, -3.0, 0.0),
    (0.789, 42.42, 0.123),
    (-0.3, 0.01, 1.0),
];

/// One recorded frame as a fixture line. `flags` must already be masked to
/// the bits that reach the wire (BRS | ESI = 0x03): the kernel-internal FDF
/// marker bit differs between producers and is not wire behavior.
pub fn format_frame(fd: bool, flags: u8, id: u32, data: &[u8]) -> String {
    let kind = if fd { "FD" } else { "CL" };
    let hex: String = data.iter().map(|b| format!("{b:02X}")).collect();
    format!("{kind}:{flags:X} {id:03X} {hex}")
}
