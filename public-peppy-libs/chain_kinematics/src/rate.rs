//! One damped resolved-rate step: the inner loop of every Cartesian controller
//! here, and the whole of a streaming jog.

use nalgebra::{Isometry3, SVector, Vector3, Vector6};

use crate::jacobian::damped_pseudo_inverse;
use crate::{Chain, Limit};

/// Damping for [`rate_step`] when the caller has no reason to pick another:
/// heavy enough to stay bounded through singular postures, light enough not to
/// visibly lag a jog step. Control paths that share a step should share this
/// too, so they cannot drift apart on damping alone.
pub const DEFAULT_DLS_LAMBDA: f64 = 0.05;

/// One damped resolved-rate joint step at `q` toward a world-frame task
/// increment: `dp_world` metres of end-effector translation and `dw_world`
/// axis-angle radians of rotation, either of which may be zero to softly hold
/// that component.
///
/// The caller caps the increments to its speed budgets; this rotates them into
/// the chain's base frame, solves `dq = J⁺(λ) ξ` with the damped pseudo-inverse
/// (bounded through singularities), scales `dq` so every joint respects its
/// velocity budget over `dt_s` while preserving direction, and clamps the result
/// into the position limits.
pub fn rate_step<const N: usize>(
    chain: &Chain<N>,
    q: &[f64; N],
    dp_world: Vector3<f64>,
    dw_world: Vector3<f64>,
    max_joint_velocity: &[f64; N],
    dt_s: f64,
    lambda: f64,
) -> [f64; N] {
    let to_base = chain.base_from_world().rotation;
    let dp = to_base * dp_world;
    let dw = to_base * dw_world;
    let twist = Vector6::new(dp.x, dp.y, dp.z, dw.x, dw.y, dw.z);
    let jacobian = chain.at(q).jacobian();
    let mut dq: SVector<f64, N> = damped_pseudo_inverse(&jacobian, lambda) * twist;
    let scale = (0..N)
        .map(|i| {
            let cap = max_joint_velocity[i] * dt_s;
            if dq[i].abs() > cap {
                cap / dq[i].abs()
            } else {
                1.0
            }
        })
        .fold(1.0_f64, f64::min);
    dq *= scale;
    let limits: [Limit; N] = chain.limits();
    std::array::from_fn(|i| limits[i].clamp(q[i] + dq[i]))
}

/// A pose difference as a world-frame twist: the translation, and the rotation
/// as an axis-angle vector. Shared by the callers that build a task error.
pub(crate) fn pose_error(
    from: &Isometry3<f64>,
    to: &Isometry3<f64>,
) -> (Vector3<f64>, Vector3<f64>) {
    let dp = to.translation.vector - from.translation.vector;
    let dr = (to.rotation * from.rotation.inverse()).to_rotation_matrix();
    let dw = dr
        .axis_angle()
        .map(|(axis, angle)| axis.into_inner() * angle)
        .unwrap_or_else(Vector3::zeros);
    (dp, dw)
}
