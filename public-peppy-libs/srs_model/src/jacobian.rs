//! Differential kinematics for the 7-DOF SRS arm.
//!
//! The maths is [`chain_kinematics::jacobian`]'s, generic over the joint count;
//! this fixes it at [`ARM_DOF`] for the consumers that speak in this arm's
//! shapes, and keeps the fixture-backed evidence that it holds on a real arm.

use crate::ARM_DOF;

/// Geometric Jacobian of the end-effector: maps joint rates (rad/s) to the EE
/// spatial twist, both in the arm base frame. Rows 0..3 are linear velocity
/// (m/s), rows 3..6 are angular velocity (rad/s).
pub type Jacobian = chain_kinematics::Jacobian<ARM_DOF>;

/// A (pseudo-)inverse of the [`Jacobian`]: maps an EE twist to joint rates.
pub type JacobianPinv = chain_kinematics::JacobianPinv<ARM_DOF>;

pub use chain_kinematics::{
    damped_pseudo_inverse, manipulability, null_space_projector, try_pseudo_inverse,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JointVec;
    use crate::fk::ForwardKinematics;
    use crate::test_support::v1_fk;
    use nalgebra::{Matrix6, SMatrix, Vector6};
    use rand::rngs::StdRng;
    use rand::{RngExt, SeedableRng};

    /// Uniform in-limit joint sample (mirrors the IK tests' sampler).
    fn sample_q(rng: &mut StdRng, fk: &ForwardKinematics) -> JointVec {
        let limits = fk.limits();
        std::array::from_fn(|i| rng.random_range(limits[i].lo..limits[i].hi))
    }

    /// EE twist between two configurations by central finite difference: the linear
    /// part from the origin delta, the angular part from the rotation delta read as
    /// a rotation vector (axis·angle).
    fn fd_twist(fk: &mut ForwardKinematics, q: &JointVec, i: usize, h: f64) -> Vector6<f64> {
        let mut q_plus = *q;
        let mut q_minus = *q;
        q_plus[i] += h;
        q_minus[i] -= h;
        let p_plus = fk.at(&q_plus).ee_pose();
        let p_minus = fk.at(&q_minus).ee_pose();
        let lin = (p_plus.translation.vector - p_minus.translation.vector) / (2.0 * h);
        let drot = p_plus.rotation * p_minus.rotation.inverse();
        let ang = drot.scaled_axis() / (2.0 * h);
        Vector6::new(lin.x, lin.y, lin.z, ang.x, ang.y, ang.z)
    }

    #[test]
    fn jacobian_matches_finite_difference() {
        for side in ["left", "right"] {
            let mut fk = v1_fk(side);
            let mut rng = StdRng::seed_from_u64(0x5A5);
            for _ in 0..200 {
                let q = sample_q(&mut rng, &fk);
                let j = fk.at(&q).jacobian();
                for i in 0..ARM_DOF {
                    let fd = fd_twist(&mut fk, &q, i, 1e-6);
                    let err = (j.column(i) - fd).norm();
                    assert!(err < 1e-5, "{side} joint {i}: column off by {err}");
                }
            }
        }
    }

    #[test]
    fn jacobian_follows_a_mounted_tool() {
        // The finite difference is taken on `ee_pose`, so it tracks whichever frame
        // the arm controls: agreement here is the Jacobian having moved onto the
        // tool with it, and the fixture's tcp frame is far enough off the tip to
        // make disagreement unmissable.
        for side in ["left", "right"] {
            let mut fk = v1_fk(side);
            fk.set_tool_link(&format!("openarm_{side}_tcp"))
                .expect("the fixture carries the tcp frame");
            let mut rng = StdRng::seed_from_u64(0x5A5);
            for _ in 0..50 {
                let q = sample_q(&mut rng, &fk);
                let j = fk.at(&q).jacobian();
                for i in 0..ARM_DOF {
                    let fd = fd_twist(&mut fk, &q, i, 1e-6);
                    let err = (j.column(i) - fd).norm();
                    assert!(err < 1e-5, "{side} joint {i}: column off by {err}");
                }
            }
        }
    }

    // Shifting the reference point multiplies the Jacobian by an adjoint, which is
    // unimodular, so `det(J Jᵀ)` and hence manipulability are unchanged. The IK
    // solver scores `ArmAnglePolicy::MaxManipulability` on the tip-based PoE
    // Jacobian while the servo runs on the tool-based one, and this is why the two
    // agree.
    #[test]
    fn manipulability_is_invariant_under_a_mounted_tool() {
        let bare = v1_fk("left");
        let mut tooled = v1_fk("left");
        tooled
            .set_tool_link("openarm_left_tcp")
            .expect("the fixture carries the tcp frame");
        let mut rng = StdRng::seed_from_u64(7);
        for _ in 0..200 {
            let q = sample_q(&mut rng, &bare);
            let w_bare = bare.at(&q).manipulability();
            let w_tool = tooled.at(&q).manipulability();
            assert!(
                (w_bare - w_tool).abs() <= 1e-9 * w_bare.max(1.0),
                "manipulability moved with the tool: {w_bare} vs {w_tool}"
            );
        }
    }

    #[test]
    fn pseudo_inverse_is_a_right_inverse() {
        let fk = v1_fk("left");
        let mut rng = StdRng::seed_from_u64(11);
        let mut checked = 0;
        for _ in 0..200 {
            let q = sample_q(&mut rng, &fk);
            // Stay off the straight-arm singularity where rank drops.
            if q[3] < 0.2 {
                continue;
            }
            let j = fk.at(&q).jacobian();
            let pinv =
                try_pseudo_inverse(&j, 1e-6).expect("non-singular config has a pseudo-inverse");
            // Right inverse: J J⁺ = I₆ (so any commanded twist is realized exactly).
            let resid = (j * pinv - Matrix6::identity()).norm();
            assert!(resid < 1e-9, "J J⁺ - I = {resid}");
            checked += 1;
        }
        assert!(checked > 100, "too few non-singular samples: {checked}");
    }

    #[test]
    fn damped_approaches_pseudo_inverse_for_small_lambda() {
        let fk = v1_fk("left");
        let q = [0.2, -0.3, 0.3, 1.0, -0.4, 0.5, 0.3];
        let j = fk.at(&q).jacobian();
        let pinv = try_pseudo_inverse(&j, 1e-9).expect("non-singular");
        let damped = damped_pseudo_inverse(&j, 1e-6);
        assert!(
            (pinv - damped).norm() < 1e-4,
            "DLS should approach J⁺ as lambda -> 0"
        );
    }

    #[test]
    fn damped_inverse_stays_finite_at_singularity() {
        // Straight arm (elbow at its 0 limit) is a kinematic singularity: the
        // Jacobian drops rank, so the plain pseudo-inverse is unavailable but DLS
        // stays bounded.
        let fk = v1_fk("left");
        let q = [0.0; ARM_DOF];
        let j = fk.at(&q).jacobian();
        assert!(
            manipulability(&j) < 1e-6,
            "straight arm should be (near) singular"
        );
        assert!(
            try_pseudo_inverse(&j, 1e-6).is_none(),
            "singular: no pseudo-inverse"
        );
        let damped = damped_pseudo_inverse(&j, 0.05);
        assert!(
            damped.norm().is_finite() && damped.norm() < 1e3,
            "DLS blew up: {}",
            damped.norm()
        );
    }

    #[test]
    fn damped_inverse_never_panics_on_degenerate_lambda() {
        // Even at a singularity, a zero / negative / non-finite lambda must yield a
        // finite inverse rather than panicking: the damping floor keeps J Jᵀ + λ²I
        // invertible.
        let fk = v1_fk("left");
        let j = fk.at(&[0.0; ARM_DOF]).jacobian();
        for lambda in [0.0, -0.05, f64::NAN, f64::INFINITY] {
            let d = damped_pseudo_inverse(&j, lambda);
            assert!(
                d.iter().all(|x| x.is_finite()),
                "lambda={lambda} gave non-finite inverse"
            );
        }
    }

    #[test]
    fn manipulability_positive_off_singularity() {
        let fk = v1_fk("left");
        let q = [0.2, -0.3, 0.3, 1.0, -0.4, 0.5, 0.3];
        let w = manipulability(&fk.at(&q).jacobian());
        assert!(
            w > 1e-4,
            "generic posture should be well-conditioned, got {w}"
        );
    }

    #[test]
    fn posed_methods_match_free_functions() {
        let fk = v1_fk("left");
        let q = [0.2, -0.3, 0.3, 1.0, -0.4, 0.5, 0.3];
        let j = fk.at(&q).jacobian();
        let want_pinv = try_pseudo_inverse(&j, 1e-9).expect("non-singular");
        let want_dls = damped_pseudo_inverse(&j, 0.05);
        let want_w = manipulability(&j);

        let posed = fk.at(&q);
        assert_eq!(posed.try_pseudo_inverse(1e-9), Some(want_pinv));
        assert_eq!(posed.damped_pseudo_inverse(0.05), want_dls);
        assert_eq!(posed.manipulability(), want_w);
    }

    #[test]
    fn null_space_projector_produces_no_ee_motion() {
        let fk = v1_fk("left");
        let q = [0.2, -0.3, 0.3, 1.0, -0.4, 0.5, 0.3];
        let j = fk.at(&q).jacobian();
        let n = null_space_projector(&j, 1e-9);
        // No EE twist from any null-space rate, N is an idempotent projector, and
        // (being built from the Moore-Penrose inverse) it is also symmetric.
        assert!((j * n).norm() < 1e-9, "J N should annihilate to zero");
        assert!((n * n - n).norm() < 1e-9, "N should be idempotent");
        assert!((n - n.transpose()).norm() < 1e-9, "N should be symmetric");
        // A concrete secondary joint rate maps to genuine joint motion that the EE
        // does not see.
        let qdot0 = SMatrix::<f64, { ARM_DOF }, 1>::from_fn(|i, _| (i as f64) - 3.0);
        let projected = n * qdot0;
        assert!(
            projected.norm() > 1e-6,
            "null-space motion should be non-trivial"
        );
        assert!(
            (j * projected).norm() < 1e-9,
            "projected motion must not move the EE"
        );
    }
}
