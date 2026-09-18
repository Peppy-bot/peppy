//! SRS geometry and Product-of-Exponentials screw data, derived **once** from
//! the FK chain at the home configuration. This is the single source of the
//! constants the IK solver needs, so IK and FK can never disagree about the
//! robot's geometry.
//!
//! All quantities are in the arm base frame. The shoulder `S`, elbow `E*`, and
//! wrist `W` centers are computed from the joint *axis lines*, never from
//! joint4's offset frame origin.

use nalgebra::{Isometry3, Matrix3, Vector3};

use crate::fk::ForwardKinematics;
use crate::{ARM_DOF, PARALLEL_SIN_EPS};

/// Constant kinematic model of one OpenArm: PoE screw data plus the SRS
/// shoulder/elbow/wrist centers and link lengths.
#[derive(Debug, Clone)]
pub(crate) struct ArmModel {
    /// Home screw axis direction of each joint (unit, base frame).
    pub axes: [Vector3<f64>; ARM_DOF],
    /// A point on each joint's axis at the home configuration (base frame).
    pub points: [Vector3<f64>; ARM_DOF],
    /// Tip (joint-7 link) pose at `q = 0`; the `M` matrix of the PoE formula. The
    /// tip, not a mounted tool: the solver takes this frame's origin as the wrist
    /// center, so it is the one frame that cannot move with a tool.
    pub home_tip: Isometry3<f64>,
    /// Shoulder center `S` = concurrency of joints 1,2,3.
    pub shoulder: Vector3<f64>,
    /// Elbow center `E*` = joint-4 axis point on the S-W line, at home.
    pub elbow_home: Vector3<f64>,
    /// Wrist center `W` = concurrency of joints 5,6,7, at home. Part of the SRS
    /// geometry record and checked by the tests; the solver derives the live wrist
    /// from the target (EE == W), so it is not read on the solve path.
    #[allow(dead_code)]
    pub wrist_home: Vector3<f64>,
    /// Upper-arm length |S-E*|.
    pub l_su: f64,
    /// Forearm length |E*-W|.
    pub l_uw: f64,
    /// Constant `world -> arm base` transform (this arm's fixed mounting). The
    /// IK/FK work in the arm base frame; this converts world/body-frame poses.
    pub base_from_world: Isometry3<f64>,
}

/// Tolerance (meters) for the SRS coincidence checks in [`ArmModel::from_fk`]:
/// the largest deviation accepted as "exactly" concurrent / intersecting before
/// a chain is rejected as non-SRS. An exactly-SRS chain sits at float roundoff
/// (~1e-12 m); a malformed URDF deviates far more.
const SRS_TOL_M: f64 = 1e-6;

/// Below this norm (meters) a line/axis direction is treated as the zero vector,
/// i.e. degenerate input with no defined direction. A magnitude floor, not an
/// angle, so unrelated to [`PARALLEL_SIN_EPS`]; the real axes are O(1).
const MIN_DIRECTION_NORM: f64 = 1e-12;

impl ArmModel {
    /// Derive the model from a loaded FK chain. Returns `Err` if the arm is not
    /// a clean SRS chain (shoulder/wrist axes not concurrent within [`SRS_TOL_M`],
    /// or the elbow axis not intersecting the shoulder-wrist line), so a non-SRS
    /// URDF fails loudly rather than panicking or yielding NaNs.
    ///
    /// The [`Arm`](crate::Arm) facade builds the FK once and calls this so the
    /// URDF is parsed a single time for both FK/dynamics and IK.
    pub fn from_fk(fk: &mut ForwardKinematics) -> Result<Self, String> {
        let fk = fk.at(&[0.0; ARM_DOF]); // pose at home; read everything off this view
        let home_tip = fk.tip_pose();
        let axes: [Vector3<f64>; ARM_DOF] = std::array::from_fn(|i| fk.axis_base(i));
        let points: [Vector3<f64>; ARM_DOF] = std::array::from_fn(|i| fk.origin_base(i));

        let shoulder = concurrency(&[
            (axes[0], points[0]),
            (axes[1], points[1]),
            (axes[2], points[2]),
        ])
        .ok_or("shoulder axes (j1-j3) are not concurrent: not an SRS arm")?;
        let wrist_home = concurrency(&[
            (axes[4], points[4]),
            (axes[5], points[5]),
            (axes[6], points[6]),
        ])
        .ok_or("wrist axes (j5-j7) are not concurrent: not an SRS arm")?;

        // The closed-form IK decomposes the shoulder/wrist rotations on orthonormal
        // spherical bases (`frame_map` inverts by transpose), so a concurrent-but-
        // oblique triplet would be accepted here and then solved wrong. Require each
        // triplet's axes to be mutually orthogonal. |cos| between two unit axes is
        // the sine of their deviation from perpendicular, so the angular
        // [`PARALLEL_SIN_EPS`] is the right gate (not the length tolerance).
        let oblique = [
            ("shoulder (j1-j3)", [0usize, 1, 2]),
            ("wrist (j5-j7)", [4, 5, 6]),
        ]
        .into_iter()
        .flat_map(|(label, [i, j, k])| [(label, i, j), (label, j, k), (label, i, k)])
        .find(|&(_, a, b)| axes[a].dot(&axes[b]).abs() > PARALLEL_SIN_EPS);
        if let Some((label, a, b)) = oblique {
            return Err(format!(
                "{label}: axes j{} and j{} are not orthogonal (|cos|={:.6}): \
                 the closed-form IK needs orthonormal spherical bases",
                a + 1,
                b + 1,
                axes[a].dot(&axes[b]).abs(),
            ));
        }

        // The closed-form IK assumes the tip origin coincides with the wrist
        // center, i.e. a zero wrist-to-tip offset (`ik::solve` takes p_w = p_d).
        // Enforce it here so a URDF whose tip sits off the wrist concurrency point
        // fails loudly rather than silently solving for the wrong wrist center. A
        // mounted tool is deliberately outside this: it offsets the *commanded*
        // frame, which `Arm::solve_ik` removes before reaching the closed form.
        let tip_offset = (home_tip.translation.vector - wrist_home).norm();
        if tip_offset > SRS_TOL_M {
            return Err(format!(
                "tip origin is {tip_offset:.4} m off the wrist center; the closed-form \
                 IK requires the tip link to sit at the j5-j7 concurrency point"
            ));
        }

        // E*: point on joint-4's axis line closest to the S-W line.
        let elbow_home =
            closest_point_on_line((points[3], axes[3]), (shoulder, wrist_home - shoulder))
                .ok_or("elbow axis (j4) is parallel to the shoulder-wrist line")?;

        // j4's axis must actually *intersect* the S-W line (SRS), not merely
        // pass near it: closest_point_on_line yields a point even for skew lines.
        let sw_dir = (wrist_home - shoulder).normalize();
        // j4 must be perpendicular to the S-W line (SRS elbow), so the elbow
        // redundancy circle the solver builds is well-defined.
        let j4_cos = axes[3].dot(&sw_dir).abs();
        if j4_cos > PARALLEL_SIN_EPS {
            return Err(format!(
                "elbow axis (j4) is not perpendicular to the shoulder-wrist line \
                 (|cos|={j4_cos:.6}): not an SRS arm"
            ));
        }
        let elbow_skew = (elbow_home - shoulder).cross(&sw_dir).norm();
        if elbow_skew > SRS_TOL_M {
            return Err(format!(
                "elbow axis (j4) misses the shoulder-wrist line by {elbow_skew:.4} m: \
                 not an SRS arm"
            ));
        }

        Ok(Self {
            axes,
            points,
            home_tip,
            shoulder,
            elbow_home,
            wrist_home,
            l_su: (shoulder - elbow_home).norm(),
            l_uw: (elbow_home - wrist_home).norm(),
            base_from_world: fk.base_from_world(),
        })
    }

    /// Convert a world/body-frame pose into the arm base frame the solver uses.
    pub fn base_pose(&self, world: &Isometry3<f64>) -> Isometry3<f64> {
        self.base_from_world * world
    }

    /// Convert an arm-base-frame pose (e.g. FK output) back into the world frame.
    pub fn world_pose(&self, base: &Isometry3<f64>) -> Isometry3<f64> {
        self.base_from_world.inverse() * base
    }
}

/// Least-squares concurrency point of a set of lines `(direction, point)`:
/// the point `p` minimizing the sum of squared perpendicular distances to
/// every line. Each line contributes the projector `I - ωωᵀ`; solving
/// `(Σ Pᵢ) p = Σ Pᵢ qᵢ` gives `p`. Returns `None` unless the lines are
/// genuinely concurrent: exactly-concurrent axes give a zero residual, but the
/// least-squares fit also exists for a merely *near*-concurrent (skew) triple,
/// so the per-line residual is checked against [`SRS_TOL_M`] before accepting.
fn concurrency(lines: &[(Vector3<f64>, Vector3<f64>)]) -> Option<Vector3<f64>> {
    let mut a = Matrix3::zeros();
    let mut b = Vector3::zeros();
    for (dir, pt) in lines {
        let w = dir.normalize();
        let proj = Matrix3::identity() - w * w.transpose();
        a += proj;
        b += proj * pt;
    }
    // `a` is singular only when all lines are parallel (no unique perpendicular
    // foot). Otherwise a fit point exists; verify every axis actually passes
    // through it, else the triple is skew (near-concurrent), not SRS.
    let p = a.try_inverse().map(|inv| inv * b)?;
    let max_residual = lines
        .iter()
        .map(|(dir, pt)| {
            let w = dir.normalize();
            let d = p - pt;
            (d - w * d.dot(&w)).norm() // perpendicular distance from p to the line
        })
        .fold(0.0_f64, f64::max);
    (max_residual <= SRS_TOL_M).then_some(p)
}

/// Point on line A `(a0 + s·da)` closest to line B `(b0 + t·db)`. Standard
/// two-line closest-approach solution; for intersecting lines it returns the
/// intersection.
fn closest_point_on_line(
    (a0, da): (Vector3<f64>, Vector3<f64>),
    (b0, db): (Vector3<f64>, Vector3<f64>),
) -> Option<Vector3<f64>> {
    // Zero-length direction: the line is undefined.
    if da.norm() < MIN_DIRECTION_NORM || db.norm() < MIN_DIRECTION_NORM {
        return None;
    }
    let da = da.normalize();
    let db = db.normalize();
    let r = a0 - b0;
    let dadb = da.dot(&db);
    let denom = 1.0 - dadb * dadb; // sin^2(angle between the lines)
    if denom.abs() < PARALLEL_SIN_EPS * PARALLEL_SIN_EPS {
        return None; // parallel lines: no unique closest point
    }
    let s = (-da.dot(&r) + dadb * db.dot(&r)) / denom;
    Some(a0 + s * da)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{v1_fk, v1_model};

    fn model() -> ArmModel {
        v1_model("left")
    }

    /// `from_fk` and the closed-form IK require the shoulder (j1-j3) and wrist
    /// (j5-j7) axes to form orthonormal bases. The raw URDF-local axes do NOT
    /// (j1 and j3 are both +Z locally); this confirms the link rotations make the
    /// base-frame axes at home orthonormal, so the orthogonality gate in `from_fk`
    /// accepts the real arm rather than rejecting it.
    #[test]
    fn fixture_shoulder_and_wrist_axes_orthonormal_at_home() {
        for side in ["left", "right"] {
            let fk = v1_fk(side);
            let posed = fk.at(&[0.0; ARM_DOF]);
            let axes: [Vector3<f64>; ARM_DOF] = std::array::from_fn(|i| posed.axis_base(i));
            for (label, [i, j, k]) in [("shoulder", [0usize, 1, 2]), ("wrist", [4, 5, 6])] {
                for idx in [i, j, k] {
                    assert!(
                        (axes[idx].norm() - 1.0).abs() < 1e-9,
                        "{side} {label} axis j{} not unit (|a|={})",
                        idx + 1,
                        axes[idx].norm(),
                    );
                }
                for (a, b) in [(i, j), (j, k), (i, k)] {
                    let dot = axes[a].dot(&axes[b]).abs();
                    println!("{side} {label}: |cos(j{}, j{})| = {dot:.2e}", a + 1, b + 1);
                    assert!(
                        dot < 1e-9,
                        "{side} {label}: j{}·j{} = {dot:.2e}, not orthogonal",
                        a + 1,
                        b + 1
                    );
                }
            }
        }
    }

    #[test]
    fn srs_centers_match_expected() {
        let m = model();
        assert!(
            (m.shoulder - Vector3::new(0.0, 0.0, 0.1225)).norm() < 1e-4,
            "S = {:?}",
            m.shoulder
        );
        assert!(
            (m.elbow_home - Vector3::new(0.0, 0.220, 0.1225)).norm() < 1e-4,
            "E* = {:?}",
            m.elbow_home
        );
        assert!(
            (m.wrist_home - Vector3::new(0.0, 0.436, 0.1225)).norm() < 1e-4,
            "W = {:?}",
            m.wrist_home
        );
    }

    #[test]
    fn link_lengths_match_expected() {
        let m = model();
        assert!((m.l_su - 0.220).abs() < 1e-4, "L_su = {}", m.l_su);
        assert!((m.l_uw - 0.216).abs() < 1e-4, "L_uw = {}", m.l_uw);
    }

    #[test]
    fn elbow_lies_on_shoulder_wrist_line() {
        // Confirms the gotcha is handled: E* sits on the S-W line, not
        // 31.5 mm below it where joint4's frame origin is.
        let m = model();
        let sw = (m.wrist_home - m.shoulder).normalize();
        let off = (m.elbow_home - m.shoulder) - (m.elbow_home - m.shoulder).dot(&sw) * sw;
        assert!(off.norm() < 1e-4, "E* off S-W line by {} m", off.norm());
    }

    #[test]
    fn joint_limits_present_and_ordered() {
        let limits = crate::test_support::v1_limits("left");
        for (i, l) in limits.iter().enumerate() {
            assert!(l.lo < l.hi, "joint {i}: {l:?}");
        }
        // Joint 4 (elbow) is one-sided per URDF: lower bound at 0.
        assert!(limits[3].lo.abs() < 1e-6, "j4 lower = {}", limits[3].lo);
    }

    #[test]
    fn concurrency_finds_common_point() {
        // Three lines through (1, 2, 3) along distinct directions intersect there.
        let p = Vector3::new(1.0, 2.0, 3.0);
        let got = concurrency(&[
            (Vector3::x(), p),
            (Vector3::y(), p + Vector3::new(0.0, 5.0, 0.0)),
            (
                Vector3::new(0.0, 0.0, 1.0),
                p + Vector3::new(0.0, 0.0, -2.0),
            ),
        ])
        .unwrap();
        assert!((got - p).norm() < 1e-9, "got {got:?}");
    }

    #[test]
    fn world_base_transform_matches_mount_and_round_trips() {
        use nalgebra::{Point3, Translation3, UnitQuaternion};
        let m = v1_model("left");
        // The arm base (link0) origin sits at the body->link0 mount in world:
        // (0, 0.031, 0.698). world->base must map that point to the origin.
        let base_origin = Point3::new(0.0, 0.031, 0.698);
        let in_base = m.base_from_world.transform_point(&base_origin);
        assert!(in_base.coords.norm() < 1e-6, "base origin -> {:?}", in_base);
        // base_pose / world_pose round-trip an arbitrary pose exactly.
        let p = Isometry3::from_parts(
            Translation3::new(0.1, -0.2, 0.6),
            UnitQuaternion::from_euler_angles(0.3, -0.4, 0.5),
        );
        let back = m.world_pose(&m.base_pose(&p));
        assert!((back.translation.vector - p.translation.vector).norm() < 1e-9);
        assert!(back.rotation.angle_to(&p.rotation) < 1e-9);
    }

    #[test]
    fn concurrency_none_for_parallel_axes() {
        // All-parallel lines have no unique perpendicular foot -> None (a non-SRS
        // shoulder/wrist; from_fk must then return Err rather than panic).
        assert!(
            concurrency(&[
                (Vector3::z(), Vector3::new(1.0, 0.0, 0.0)),
                (Vector3::z(), Vector3::new(0.0, 1.0, 0.0)),
                (Vector3::z(), Vector3::new(2.0, 3.0, 0.0)),
            ])
            .is_none()
        );
    }

    #[test]
    fn concurrency_accepts_exactly_concurrent() {
        // Three axes through a common point return that point.
        let c = Vector3::new(0.2, -0.3, 0.5);
        let p = concurrency(&[(Vector3::x(), c), (Vector3::y(), c), (Vector3::z(), c)]).unwrap();
        assert!((p - c).norm() < 1e-9, "got {p:?}");
    }

    #[test]
    fn concurrency_rejects_near_concurrent_skew() {
        // Axes that do NOT share a point: the least-squares fit exists (not
        // all-parallel) but its residual exceeds SRS_TOL_M, so it is rejected
        // rather than accepted as a spurious shoulder/wrist center.
        assert!(
            concurrency(&[
                (Vector3::x(), Vector3::zeros()),
                (Vector3::y(), Vector3::zeros()),
                (Vector3::z(), Vector3::new(0.1, 0.0, 0.0)), // 0.1 m off the origin
            ])
            .is_none()
        );
    }

    #[test]
    fn closest_point_on_line_returns_intersection() {
        // Line A: x-axis through origin; Line B: y-axis through (2,0,0).
        // The closest point on A is the intersection (2, 0, 0).
        let got = closest_point_on_line(
            (Vector3::zeros(), Vector3::x()),
            (Vector3::new(2.0, 0.0, 0.0), Vector3::y()),
        )
        .unwrap();
        assert!(
            (got - Vector3::new(2.0, 0.0, 0.0)).norm() < 1e-9,
            "got {got:?}"
        );
    }

    #[test]
    fn closest_point_none_for_parallel_lines() {
        assert!(
            closest_point_on_line(
                (Vector3::zeros(), Vector3::x()),
                (Vector3::new(0.0, 1.0, 0.0), Vector3::x()),
            )
            .is_none()
        );
    }
}
