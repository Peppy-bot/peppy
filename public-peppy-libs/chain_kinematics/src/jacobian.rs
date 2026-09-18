//! Differential kinematics: the geometric Jacobian's inverses, generic over the
//! number of joints.
//!
//! A chain has `N` joints but an end-effector twist is 6-dimensional, so the
//! Jacobian is **6xN**. When `N > 6` it has no square inverse and the surplus
//! degrees of freedom are a null space to be resolved; when `N < 6` not every
//! twist is achievable and these give the least-squares answer. Two inverses
//! cover the usual control schemes:
//!
//! - [`try_pseudo_inverse`] is the Moore-Penrose (minimum-norm) right inverse,
//!   returning `None` at a singularity where the Jacobian drops rank.
//! - [`damped_pseudo_inverse`] is the damped-least-squares inverse, always
//!   defined, trading tracking accuracy for conditioning near singularities.
//!   This is the one a resolved-rate loop runs every tick.
//!
//! [`manipulability`] reports proximity to a singularity, and
//! [`null_space_projector`] maps a secondary joint-rate objective (posture,
//! joint-limit avoidance, elbow placement) into the Jacobian's null space so it
//! does not disturb the end effector.
//!
//! All quantities are in the frame the Jacobian was taken in.

use nalgebra::{DMatrix, Matrix6, SMatrix};

// nalgebra's SVD is written against type-level integers and cannot be named over
// a generic `Const<N>`, so the two decomposing functions below go through a
// dynamically-sized matrix. That costs an allocation, which is why neither is on
// the control path: a resolved-rate tick runs `damped_pseudo_inverse`, which is
// a fixed-size Cholesky and allocates nothing.

/// Floor on `λ²` in [`damped_pseudo_inverse`], keeping `J Jᵀ + λ²I` strictly
/// positive-definite (hence invertible) even if a caller passes `lambda = 0` or a
/// non-finite value. Negligible against any real damping (`lambda ~ 1e-2`), so it
/// only guards the degenerate input, never alters intended behavior.
const MIN_DAMPING_SQ: f64 = 1e-12;

/// Geometric Jacobian of the end-effector: maps joint rates (rad/s) to the EE
/// spatial twist, both in the arm base frame. Rows 0..3 are linear velocity
/// (m/s), rows 3..6 are angular velocity (rad/s).
pub type Jacobian<const N: usize> = SMatrix<f64, 6, N>;

/// A (pseudo-)inverse of the [`Jacobian`]: maps an EE twist to joint rates.
pub type JacobianPinv<const N: usize> = SMatrix<f64, N, 6>;

/// Moore-Penrose (minimum-norm) right inverse `J⁺ = Jᵀ (J Jᵀ)⁻¹`, the joint rates
/// of least norm that realize a commanded EE twist. Returns `None` whenever no
/// such rates exist: at (or near) a singularity, where the smallest singular
/// value is `<= eps` and the solution is ill-conditioned, and on a chain of
/// fewer than six joints, which cannot meet an arbitrary six-dimensional twist
/// in the first place. Use [`damped_pseudo_inverse`] in both cases, which
/// answers everywhere at the cost of some tracking error.
pub fn try_pseudo_inverse<const N: usize>(j: &Jacobian<N>, eps: f64) -> Option<JacobianPinv<N>> {
    // An under-actuated chain has no right inverse, and its rank never announces
    // that: a 6xN Jacobian with N < 6 has only N singular values, so the missing
    // task directions never show up as small ones and the guard below cannot see
    // them. Answering here would hand back a least-squares fit that does not
    // realize the commanded twist, with nothing to say so.
    if N < 6 {
        return None;
    }
    let svd = DMatrix::from_iterator(6, N, j.iter().copied()).svd(true, true);
    // A full-row-rank 6xN Jacobian has 6 singular values; the smallest gauges how
    // close it is to losing rank. Guarding on it (rather than letting the SVD
    // silently zero small values) is what makes this the fallible variant.
    let s_min = svd
        .singular_values
        .iter()
        .copied()
        .fold(f64::INFINITY, f64::min);
    if s_min <= eps {
        return None;
    }
    let pinv = svd.pseudo_inverse(eps).ok()?;
    Some(JacobianPinv::<N>::from_iterator(pinv.iter().copied()))
}

/// Damped-least-squares inverse `J⁺ = Jᵀ (J Jᵀ + λ²I)⁻¹`. Unlike
/// [`try_pseudo_inverse`] it is defined everywhere, including at singularities: the
/// damping bounds the joint rates at the cost of some tracking error. A
/// resolved-rate controller runs this every tick; pick `lambda` (~1e-2) to trade
/// tracking accuracy against rate magnitude near singularities.
///
/// Only `λ²` enters, so the sign of `lambda` is irrelevant. A zero or non-finite
/// `lambda` is clamped to a negligible internal floor so the result stays defined
/// rather than inverting a singular matrix; that floor is not a meaningful damping,
/// so pass a genuine value. This inverse never fails.
pub fn damped_pseudo_inverse<const N: usize>(j: &Jacobian<N>, lambda: f64) -> JacobianPinv<N> {
    // λ² must be strictly positive for J Jᵀ + λ²I to be SPD; clamp a zero or
    // non-finite lambda to the floor so Cholesky always succeeds and this stays
    // infallible.
    let lambda2 = if lambda.is_finite() {
        (lambda * lambda).max(MIN_DAMPING_SQ)
    } else {
        MIN_DAMPING_SQ
    };
    let jt = j.transpose();
    let damped: Matrix6<f64> = j * jt + Matrix6::identity() * lambda2;
    let inv = damped
        .cholesky()
        .expect("J Jᵀ + λ²I is SPD for λ² > 0")
        .inverse();
    jt * inv
}

/// Yoshikawa manipulability index, the product of the Jacobian's singular
/// values: a scalar measure of how far the posture is from a singularity (0
/// exactly at one, larger is better-conditioned). Useful for monitoring a
/// control loop or steering the redundancy away from singular regions.
///
/// Read off whichever Gram matrix is the smaller: `√det(J Jᵀ)` once the chain
/// has six joints or more, and `√det(Jᵀ J)` below that, where `J Jᵀ` is 6x6 of
/// rank at most `N` and so has determinant zero at every posture, singular or
/// not. The two agree on a square Jacobian, and both are the product of the
/// singular values, so the number means the same thing at any joint count.
///
/// The `N < 6` branch goes through a dynamically-sized matrix, which nalgebra
/// needs to take an `NxN` determinant, and so allocates; the six-and-above
/// branch is fixed-size throughout.
pub fn manipulability<const N: usize>(j: &Jacobian<N>) -> f64 {
    // The determinant is non-negative in exact arithmetic; clamp away rounding
    // noise that can make a near-singular value slightly negative before the sqrt.
    let gram = if N < 6 {
        let dynamic = DMatrix::from_iterator(6, N, j.iter().copied());
        (dynamic.transpose() * dynamic).determinant()
    } else {
        (j * j.transpose()).determinant()
    };
    gram.max(0.0).sqrt()
}

/// Exact null-space projector `N = I − J⁺J` for the redundant DOF, where `J⁺` is
/// the Moore-Penrose inverse of `j`. Joint rates `N q̇` produce no end-effector
/// motion, so a secondary objective (posture, joint-limit avoidance, elbow
/// placement) can be added as `task + N q̇₀` without disturbing the commanded
/// twist. `N` is a true orthogonal projector (symmetric and idempotent).
///
/// It is built from the Moore-Penrose inverse on purpose: `I − J_dls⁺J` from a
/// damped inverse is only an *approximate* projector and leaks secondary motion
/// into the twist, so even a damped-least-squares controller should track its task
/// with the damped inverse but project the secondary term with this. `eps` is the
/// singular-value rank tolerance; the projector stays exact at singularities, where
/// the null space simply grows.
pub fn null_space_projector<const N: usize>(j: &Jacobian<N>, eps: f64) -> SMatrix<f64, N, N> {
    let pinv = DMatrix::from_iterator(6, N, j.iter().copied())
        .svd(true, true)
        .pseudo_inverse(eps)
        .expect("SVD pseudo-inverse only fails for eps < 0");
    SMatrix::<f64, N, N>::identity() - JacobianPinv::<N>::from_iterator(pinv.iter().copied()) * j
}

use crate::Posed;

impl<const N: usize> Posed<'_, N> {
    /// Minimum-norm pseudo-inverse of this posture's Jacobian; `None` at a
    /// singularity. Convenience for the one-shot case: when the Jacobian is
    /// also needed, compute it once and use the free functions.
    pub fn try_pseudo_inverse(&self, eps: f64) -> Option<JacobianPinv<N>> {
        try_pseudo_inverse(&self.jacobian(), eps)
    }

    /// Damped-least-squares inverse of this posture's Jacobian, defined
    /// everywhere including singularities.
    pub fn damped_pseudo_inverse(&self, lambda: f64) -> JacobianPinv<N> {
        damped_pseudo_inverse(&self.jacobian(), lambda)
    }

    /// Yoshikawa manipulability of this posture.
    pub fn manipulability(&self) -> f64 {
        manipulability(&self.jacobian())
    }
}
