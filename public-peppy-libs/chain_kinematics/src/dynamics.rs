//! Feedforward dynamics over a posed chain: gravity-compensation torques and
//! the Coriolis/centripetal vector `C(q, q̇)·q̇`, read off [`Posed`] like every
//! other pose-dependent quantity.
//!
//! Both work in the world frame, because gravity is a world quantity (it acts
//! along world -Z), and both dispatch on [`JointKind`]: a revolute joint takes
//! moments about its axis, a prismatic joint takes forces along it. Any distal
//! payload past the tip is already lumped into the last segment at load, so it
//! is carried here without special cases.

use nalgebra::Vector3;

use crate::Posed;
use crate::tree::JointKind;

/// Standard gravity (m/s^2), along world -Z. URDF poses its trees Z-up, so the
/// convention is the description format's, not a robot's.
const STANDARD_GRAVITY: f64 = 9.81;

impl<const N: usize> Posed<'_, N> {
    /// Gravity-compensation torques: what each joint must apply to hold the
    /// chain still against gravity at this configuration. Equal to the gradient
    /// of the potential energy in `q`, which is what the tests check it
    /// against.
    pub fn gravity_torques(&self) -> [f64; N] {
        // Joint j carries every segment at or below it on the chain. Gravity on
        // segment i is (0, 0, -m·g); a revolute joint feels its moment about
        // the axis, which reduces to m·g·(axis × r).z because gravity has only
        // a z component, and a prismatic joint feels the force along its axis,
        // m·g·axis.z.
        std::array::from_fn(|j| {
            let origin_j = self.origin_world(j);
            let axis_j = self.axis_world(j);
            self.distal_from(j)
                .map(|i| {
                    let weight = self.mass(i) * STANDARD_GRAVITY;
                    match self.kind(j) {
                        JointKind::Revolute { .. } => {
                            let r = self.com_world(i) - origin_j;
                            weight * axis_j.cross(&r).z
                        }
                        JointKind::Prismatic { .. } => weight * axis_j.z,
                        JointKind::Fixed => 0.0,
                    }
                })
                .sum()
        })
    }

    /// Coriolis + centripetal torques at velocity `q̇`: the `C(q, q̇)·q̇` vector
    /// of the manipulator equation `M(q)q̈ + C(q, q̇)q̇ + g(q) = τ`.
    ///
    /// A world-frame recursive Newton-Euler pass with `q̈ = 0` and gravity off,
    /// so only the velocity coupling remains; no mass matrix is materialized.
    pub fn coriolis_torques(&self, qdot: &[f64; N]) -> [f64; N] {
        // Both passes walk the chain proximal to distal, which is not `0..N`:
        // the order of `q` is the caller's. Everything is stored by the entry of
        // `q` it belongs to, so the torques come back in the caller's order.
        let order = self.proximal_order();

        // Forward pass: propagate angular velocity, angular acceleration, and
        // the linear acceleration of each joint origin and link COM outward,
        // starting from a fixed base (zeros).
        // `slide` is a prismatic joint's linear rate along its axis, zero at a
        // revolute joint; it enters the accelerations twice, as the Coriolis
        // term of the sliding frame and as the rotation of the slide direction.
        let mut omega = [Vector3::<f64>::zeros(); N];
        let mut alpha = [Vector3::<f64>::zeros(); N];
        let mut a_origin = [Vector3::<f64>::zeros(); N];
        let mut a_com = [Vector3::<f64>::zeros(); N];

        let mut omega_parent = Vector3::<f64>::zeros();
        let mut alpha_parent = Vector3::<f64>::zeros();
        let mut a_parent = Vector3::<f64>::zeros();
        let mut slide_parent = Vector3::<f64>::zeros();
        let mut prev_origin = Vector3::<f64>::zeros();
        for &i in order.iter() {
            let origin = self.origin_world(i);
            let axis = self.axis_world(i);

            // Joint i's origin is rigidly set on the segment above it (plus that
            // segment's slide, if a prismatic joint drives it, whose rate rides
            // along as `slide_parent`), so its acceleration comes from the
            // parent's (ω, α) and the Coriolis of that slide.
            let r = origin - prev_origin;
            let a_joint = a_parent
                + alpha_parent.cross(&r)
                + omega_parent.cross(&omega_parent.cross(&r))
                + 2.0 * omega_parent.cross(&slide_parent);

            let (qd_spin, qd_slide) = match self.kind(i) {
                JointKind::Revolute { .. } => (qdot[i] * axis, Vector3::zeros()),
                JointKind::Prismatic { .. } => (Vector3::zeros(), qdot[i] * axis),
                JointKind::Fixed => (Vector3::zeros(), Vector3::zeros()),
            };
            omega[i] = omega_parent + qd_spin;
            // Full RNEA: α_child = α + ω × q̇·ĥ + q̈·ĥ. With q̈ = 0 only the
            // parent-ω cross-coupling with this joint's own rate survives; for
            // a prismatic joint the axis itself rotates with the parent, which
            // is the same cross term applied to the slide.
            alpha[i] = alpha_parent + omega_parent.cross(&qd_spin);
            a_origin[i] = a_joint;

            // COM acceleration adds this link's own angular contribution and
            // the Coriolis of its own slide.
            let c = self.com_world(i) - origin;
            a_com[i] = a_joint
                + alpha[i].cross(&c)
                + omega[i].cross(&omega[i].cross(&c))
                + 2.0 * omega[i].cross(&qd_slide);

            omega_parent = omega[i];
            alpha_parent = alpha[i];
            a_parent = a_joint;
            slide_parent = qd_slide;
            prev_origin = origin;
        }

        // Backward pass: accumulate the inertial force and moment each parent
        // must transmit to its child, from the tip inward. A revolute joint's
        // torque is the moment's projection on its axis; a prismatic joint's
        // force is the force's projection.
        let mut f_child = Vector3::<f64>::zeros();
        let mut n_child = Vector3::<f64>::zeros();
        let mut tau = [0.0_f64; N];

        for (rank, &i) in order.iter().enumerate().rev() {
            let origin = self.origin_world(i);
            let inertia = self.inertia_world(i);

            let force_com = self.mass(i) * a_com[i];
            let moment_com = inertia * alpha[i] + omega[i].cross(&(inertia * omega[i]));

            let force_joint = force_com + f_child;

            let r_com = self.com_world(i) - origin;
            let r_child = match order.get(rank + 1) {
                Some(&child) => self.origin_world(child) - origin,
                // Tip link has no child: f_child and n_child are zero.
                None => Vector3::zeros(),
            };
            let moment_joint =
                moment_com + r_com.cross(&force_com) + n_child + r_child.cross(&f_child);

            tau[i] = match self.kind(i) {
                JointKind::Revolute { .. } => self.axis_world(i).dot(&moment_joint),
                JointKind::Prismatic { .. } => self.axis_world(i).dot(&force_joint),
                JointKind::Fixed => 0.0,
            };

            f_child = force_joint;
            n_child = moment_joint;
        }

        tau
    }
}
