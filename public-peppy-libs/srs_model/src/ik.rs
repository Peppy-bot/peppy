//! Closed-form arm-angle inverse kinematics for a clean SRS (spherical-revolute-
//! spherical) 7-DOF arm.
//!
//! The geometry drives everything. The shoulder (joints 1-2-3) is concurrent at
//! `S`, the wrist (joints 5-6-7) at `W`, joint 4 is the elbow between them, and
//! the EE origin coincides with `W` (enforced in [`crate::model::ArmModel`]). A
//! 6-DOF target leaves one redundant DOF: with `S`, `W`, and the elbow flex
//! fixed, the elbow is still free to swing on a circle about the `S`-`W` line.
//! The angle around that circle is the **arm angle** `psi`.
//!
//! So a solve is just geometry:
//!   1. the wrist center is the target position (EE == `W`);
//!   2. the elbow flex `theta4` follows from the `S`-`W` distance (law of cosines);
//!   3. `psi` places the elbow on the circle, which fixes the upper-arm pose and so
//!      the shoulder rotation `R_s`, leaving a residual wrist rotation `R_w`;
//!   4. `R_s` decomposes into `theta1..3` and `R_w` into `theta5..7`.
//!
//! Step 4, splitting a rotation into three fixed-axis rotations, is the only
//! non-obvious part. It uses the **Paden-Kahan subproblems**
//! ([`subproblem1`]/[`subproblem2`]): standard, closed-form geometric primitives
//! for "what angle about this axis carries p onto q". They take the joint axes
//! read straight from the FK-validated [`crate::model::ArmModel`], so signs and
//! offsets are correct by construction (no DH transcription), the two discrete
//! branches fall out of the algebra, and the same code handles the mirror arm and
//! any other SRS URDF unchanged.

use std::f64::consts::{FRAC_PI_2, PI, TAU};

use nalgebra::{Isometry3, Matrix3, Rotation3, Unit, Vector3, Vector6};

use crate::jacobian::{Jacobian, manipulability};

use crate::model::ArmModel;
use crate::{ARM_DOF, JointVec, Limit, PARALLEL_SIN_EPS};

/// How to resolve the redundant arm angle when the caller doesn't pin it.
#[derive(Debug, Clone, Copy)]
pub enum ArmAnglePolicy {
    /// Use the arm angle of the seed configuration (continuity; servoing).
    FromSeed,
    /// Use this exact arm angle (radians). Infeasible if it violates a limit.
    Fixed(f64),
    /// Start from the seed's arm angle (like `FromSeed`) and step at most
    /// `max_step_rad` toward higher manipulability, so a solve chain steers the
    /// elbow away from singular postures instead of freezing it. The bounded step
    /// keeps consecutive solutions continuous; a chain of solves follows the local
    /// manipulability ridge as the target moves. `max_step_rad = 0` reduces to
    /// `FromSeed`. Solvable exactly when `FromSeed` is.
    MaxManipulability { max_step_rad: f64 },
}

/// One inverse-kinematics solution.
#[derive(Debug, Clone)]
pub struct Solution {
    pub q: JointVec,
    /// The arm angle the solution was built at.
    pub arm_angle: f64,
}

// ---------------------------------------------------------------------------
// Rotation / screw primitives, plus an independent forward map built by
// composing the per-joint screws. It must agree with the k-chain FK (see tests),
// which is what validates the axes/points the IK trusts.
// ---------------------------------------------------------------------------

fn exp_so3(axis: Vector3<f64>, angle: f64) -> Rotation3<f64> {
    Rotation3::from_axis_angle(&Unit::new_normalize(axis), angle)
}

/// Pure rotation by `angle` about the line through `point` with direction
/// `axis`, as a homogeneous point map `p -> R(p - point) + point`.
fn screw_point(
    axis: Vector3<f64>,
    point: Vector3<f64>,
    angle: f64,
    p: Vector3<f64>,
) -> Vector3<f64> {
    let r = exp_so3(axis, angle);
    r * (p - point) + point
}

/// Forward kinematics from the PoE screw data: EE position in the base frame.
/// Independent path from the `k`-chain FK; the two must agree (see tests),
/// which validates the screw axes/points the IK relies on.
pub(crate) fn fk_poe_position(model: &ArmModel, q: &JointVec) -> Vector3<f64> {
    let mut p = model.home_tip.translation.vector;
    for i in (0..ARM_DOF).rev() {
        p = screw_point(model.axes[i], model.points[i], q[i], p);
    }
    p
}

/// Full PoE forward rotation: `exp(w1 q1)...exp(w7 q7) * R_home`. Only the
/// rotation cross-check in the tests needs it (the production path uses the
/// position map via `arm_angle_of`).
#[cfg(test)]
pub(crate) fn fk_poe_rotation(model: &ArmModel, q: &JointVec) -> Rotation3<f64> {
    let mut r = model.home_tip.rotation.to_rotation_matrix();
    for i in (0..ARM_DOF).rev() {
        r = exp_so3(model.axes[i], q[i]) * r;
    }
    r
}

// ---------------------------------------------------------------------------
// Paden-Kahan subproblems (axes through a common point taken as the origin;
// callers pass vectors already relative to that point).
// ---------------------------------------------------------------------------

/// Subproblem 1: angle `theta` with `exp(axis, theta) p = q` (rotation about a
/// line through the origin). `p`, `q` need not be perpendicular to `axis`; only
/// their components in the rotation plane are used.
fn subproblem1(axis: Vector3<f64>, p: Vector3<f64>, q: Vector3<f64>) -> f64 {
    let u = p - axis * axis.dot(&p);
    let v = q - axis * axis.dot(&q);
    f64::atan2(axis.dot(&u.cross(&v)), u.dot(&v))
}

/// Numerically-zero slack for the O(1) algebraic quantities in the closed-form
/// solvers: the subproblem-2 discriminant `gamma`(²) (below it the two branches
/// coincide) and, in [`sincos_roots`], the `a sin + b cos` amplitude and the
/// `|cos|`-past-1 roundoff. All are O(1) for this arm's orthogonal axes, so this
/// is a roundoff floor, not a degeneracy angle.
const ALGEBRAIC_EPS: f64 = 1e-9;

/// Minimum `sin(angle)` between the probe axis and `w3` in [`decompose_three_axes`].
/// Deliberately coarser than [`PARALLEL_SIN_EPS`]: we want a *well-conditioned*
/// probe to normalize, not merely a non-degenerate one, and we fall back to a
/// second axis when the first is too aligned.
const PROBE_AXIS_SIN_MIN: f64 = 1e-3;

/// Subproblem 2: solve `exp(w1, t1) exp(w2, t2) p = q` for axes `w1`, `w2`
/// intersecting at the origin. Returns up to two `(t1, t2)` branches.
fn subproblem2(
    w1: Vector3<f64>,
    w2: Vector3<f64>,
    p: Vector3<f64>,
    q: Vector3<f64>,
) -> Vec<(f64, f64)> {
    let w1w2 = w1.dot(&w2);
    let denom = w1w2 * w1w2 - 1.0; // -sin^2(angle between the axes)
    if denom.abs() < PARALLEL_SIN_EPS * PARALLEL_SIN_EPS {
        // Degenerate-input guard: w1 parallel to w2. The shoulder/wrist are always
        // decomposed about their fixed orthogonal home axes, so this never fires
        // for this arm; it protects against a non-SRS chain being fed in.
        return Vec::new();
    }
    let alpha = (w1w2 * w2.dot(&p) - w1.dot(&q)) / denom;
    let beta = (w1w2 * w1.dot(&q) - w2.dot(&p)) / denom;
    let cross = w1.cross(&w2);
    let cross_n2 = cross.norm_squared();
    let gamma_sq =
        (p.norm_squared() - alpha * alpha - beta * beta - 2.0 * alpha * beta * w1w2) / cross_n2;
    if gamma_sq < -ALGEBRAIC_EPS {
        return Vec::new();
    }
    let gamma = gamma_sq.max(0.0).sqrt();
    let branch = |g: f64| {
        let z = alpha * w1 + beta * w2 + g * cross;
        let t2 = subproblem1(w2, p, z);
        let t1 = subproblem1(w1, z, q);
        (t1, t2)
    };
    if gamma < ALGEBRAIC_EPS {
        vec![branch(0.0)]
    } else {
        vec![branch(gamma), branch(-gamma)]
    }
}

/// Decompose a rotation into three fixed-axis rotations:
/// `R = exp(w1, t1) exp(w2, t2) exp(w3, t3)`. Returns up to two branches.
/// Works because `exp(w3, t3)` fixes `w3`, so `R w3 = exp(w1,t1) exp(w2,t2) w3`
/// is a subproblem 2 for `(t1, t2)`; `t3` then follows from subproblem 1.
fn decompose_three_axes(
    r: &Rotation3<f64>,
    w1: Vector3<f64>,
    w2: Vector3<f64>,
    w3: Vector3<f64>,
) -> Vec<(f64, f64, f64)> {
    let d1 = r * w3;
    // pick a probe vector not parallel to w3
    let seed = if w3.cross(&Vector3::x()).norm() > PROBE_AXIS_SIN_MIN {
        Vector3::x()
    } else {
        Vector3::y()
    };
    let v = (seed - w3 * w3.dot(&seed)).normalize();
    subproblem2(w1, w2, w3, d1)
        .into_iter()
        .map(|(t1, t2)| {
            let rc = exp_so3(w2, t2).inverse() * exp_so3(w1, t1).inverse() * r;
            let t3 = subproblem1(w3, v, rc * v);
            (t1, t2, t3)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Solver
// ---------------------------------------------------------------------------

/// Float slack on the reach interval so a target sitting essentially *at* the
/// max-reach (straight arm) or min-reach boundary is rejected rather than driven
/// into the ill-conditioned region just inside it. The exact straight-arm pose is
/// also caught downstream by the arm-plane normal check ([`PARALLEL_SIN_EPS`])
/// and, for seeds, by [`SINGULAR_RADIUS`].
const REACH_EPS: f64 = 1e-9;

/// Numerical floor (m) on the elbow-circle radius below which the arm angle is
/// taken as undefined. The arm angle is the *direction* of the elbow off the
/// shoulder-wrist line, so it stays well-defined and numerically stable for any
/// physically non-straight configuration even when that offset is sub-millimetre
/// (a near-straight seed still carries a usable arm angle, e.g. the home pose at
/// q4 ≈ 0.05 has a ~5 mm radius and a clean angle). Only at the exact straight-arm
/// pose (radius → 0, elbow on the line) is the angle genuinely undefined; this
/// floor guards just that degenerate case, where the offset direction is lost to
/// rounding.
const SINGULAR_RADIUS: f64 = 1e-6;

/// Arm angle of configuration `q`: the angle of its elbow on the redundancy
/// circle about the S-W line, measured from the reference direction. `None` only
/// at the exact straight-arm singularity (elbow-circle radius below
/// [`SINGULAR_RADIUS`]), where the elbow sits on the S-W line and the angle is
/// geometrically undefined (returning a fabricated 0.0 there would mislead
/// callers). A merely near-straight `q` still has a well-defined angle.
pub(crate) fn arm_angle_of(model: &ArmModel, q: &JointVec) -> Option<f64> {
    let circle = circle_frame(model, fk_poe_position(model, q));
    circle.azimuth(elbow_position(model, q))
}

/// Elbow center of configuration `q`: joints 1-3 carry the home elbow about S;
/// joint 4+ leave it fixed (it lies on joint 4's axis).
fn elbow_position(model: &ArmModel, q: &JointVec) -> Vector3<f64> {
    let mut e = model.elbow_home;
    for i in (0..3).rev() {
        e = screw_point(model.axes[i], model.points[i], q[i], e);
    }
    e
}

/// The elbow redundancy circle for a wrist center: the elbow rides this circle
/// about the S-W line as the arm angle varies. Depends only on the wrist center,
/// so it is computed once per solve and reused across all arm angles.
struct Circle {
    radius: f64,
    center: Vector3<f64>,
    /// `psi = 0` reference direction (in the circle plane).
    a_hat: Vector3<f64>,
    /// `psi = π/2` direction; `(a_hat, b_hat, n_hat)` is right-handed.
    b_hat: Vector3<f64>,
}

impl Circle {
    fn elbow(&self, psi: f64) -> Vector3<f64> {
        self.center + self.radius * (psi.cos() * self.a_hat + psi.sin() * self.b_hat)
    }

    /// Azimuth of a point about this circle's axis, in the `(a_hat, b_hat)`
    /// basis: the inverse of [`elbow`](Self::elbow) for a point on the circle,
    /// and the projected direction for one off it (a seed's elbow measured on
    /// another configuration's circle). `None` when the point projects onto the
    /// axis itself, where no direction is defined; for a point on the circle
    /// that is exactly the [`SINGULAR_RADIUS`] straight-arm test.
    fn azimuth(&self, p: Vector3<f64>) -> Option<f64> {
        let e = p - self.center;
        let (a, b) = (e.dot(&self.a_hat), e.dot(&self.b_hat));
        (a.hypot(b) >= SINGULAR_RADIUS).then(|| f64::atan2(b, a))
    }
}

fn circle_frame(model: &ArmModel, p_w: Vector3<f64>) -> Circle {
    let s = model.shoulder;
    let n = p_w - s;
    // d is bounded below by the arm's minimum reach |l_su - l_uw| > 0 for any
    // joint configuration, so n_hat is well defined (callers pass a reachable
    // wrist center: either FK of a real q, or a target past solve()'s reach check).
    let d = n.norm();
    let n_hat = n / d;
    let h = (model.l_su * model.l_su - model.l_uw * model.l_uw + d * d) / (2.0 * d);
    let center = s + h * n_hat;
    let radius = (model.l_su * model.l_su - h * h).max(0.0).sqrt();
    // psi=0 reference: project the coordinate axis least aligned with the S-W
    // line onto the circle plane. The least-aligned axis is at most 54.7° off the
    // plane, so a_hat is always well-conditioned (no thin near-vertical band).
    let (ax, ay, az) = (
        n_hat.dot(&Vector3::x()).abs(),
        n_hat.dot(&Vector3::y()).abs(),
        n_hat.dot(&Vector3::z()).abs(),
    );
    let r0 = if ax <= ay && ax <= az {
        Vector3::x()
    } else if ay <= az {
        Vector3::y()
    } else {
        Vector3::z()
    };
    let a_hat = (r0 - n_hat * n_hat.dot(&r0)).normalize();
    let b_hat = n_hat.cross(&a_hat);
    Circle {
        radius,
        center,
        a_hat,
        b_hat,
    }
}

/// Solve inverse kinematics for a `target` EE pose in the arm base frame,
/// resolving redundancy per `arm_angle`. `seed` selects the discrete branch (and
/// the arm angle when `FromSeed`). The target is the same [`Isometry3`] type the
/// forward kinematics emit, so an FK pose feeds straight back in; convert a
/// world-frame target first with [`ArmModel::base_pose`].
///
/// In `FromSeed` mode the seed's arm angle is used when it is feasible
/// (continuity); otherwise the closest feasible arm angle is taken from the
/// exact feasible set, so a solution is still returned when the seed's angle is
/// infeasible for this target, or when the seed is at the exact straight-arm
/// singularity and so carries no arm angle (a merely near-straight seed, e.g. the
/// home pose at q4 ≈ 0.05, still does and is tracked continuously). In `Fixed`
/// mode the given arm angle is used verbatim and infeasibility yields `None`.
pub(crate) fn solve(
    model: &ArmModel,
    limits: &[Limit; ARM_DOF],
    target: &Isometry3<f64>,
    arm_angle: ArmAnglePolicy,
    seed: &JointVec,
) -> Option<Solution> {
    let r_d = target.rotation.to_rotation_matrix();
    let p_d = target.translation.vector;
    if !(p_d.x.is_finite() && p_d.y.is_finite() && p_d.z.is_finite()) {
        return None; // reject NaN/Inf position up front
    }
    if !r_d.matrix().iter().all(|x| x.is_finite()) {
        return None; // reject NaN/Inf orientation up front
    }
    let p_w = p_d; // EE origin coincides with the wrist center
    let d = (p_w - model.shoulder).norm();
    if d > model.l_su + model.l_uw - REACH_EPS || d < (model.l_su - model.l_uw).abs() + REACH_EPS {
        return None; // unreachable / straight-arm singular boundary
    }

    // theta4: elbow flex from reach alone.
    let cos4 = ((d * d - model.l_su * model.l_su - model.l_uw * model.l_uw)
        / (2.0 * model.l_su * model.l_uw))
        .clamp(-1.0, 1.0);
    let theta4 = cos4.acos();

    // The elbow circle depends only on the wrist center, so compute it once and
    // reuse it across all arm angles.
    let ctx = PsiSolve {
        model,
        limits,
        r_d,
        p_w,
        theta4,
        circle: circle_frame(model, p_w),
    };
    match arm_angle {
        ArmAnglePolicy::Fixed(psi) => ctx
            .solve_at_psi(psi, seed)
            .map(|q| Solution { q, arm_angle: psi }),
        ArmAnglePolicy::FromSeed => {
            // The redundancy circle is resolved analytically: every joint angle is
            // a closed-form function of psi, so each joint limit maps to an exact
            // feasible-psi interval. Pick the feasible psi nearest the seed's arm
            // angle for continuity, then build q there.
            let preferred = ctx.preferred_psi(seed).unwrap_or(0.0);
            let intervals = ctx.feasible_psi_intervals(seed);
            let psi = nearest_feasible_psi(&intervals, preferred)?;
            ctx.solve_at_psi(psi, seed)
                .map(|q| Solution { q, arm_angle: psi })
        }
        ArmAnglePolicy::MaxManipulability { max_step_rad } => {
            // Anchor at the FromSeed angle (continuity, guaranteed feasible), then
            // step within the bounded window toward higher manipulability.
            let preferred = ctx.preferred_psi(seed).unwrap_or(0.0);
            let intervals = ctx.feasible_psi_intervals(seed);
            let psi0 = nearest_feasible_psi(&intervals, preferred)?;
            let (psi, q) = ctx.max_manipulability_step(psi0, max_step_rad, seed)?;
            Some(Solution {
                q,
                arm_angle: wrap_pi(psi),
            })
        }
    }
}

/// Everything about one solve that is fixed once the target passes the reach
/// check: the model, the decomposed target (rotation and wrist center), the elbow
/// flex, and the redundancy circle. The per-psi machinery hangs off it as
/// methods, so each varies only its psi/seed rather than rethreading the target.
struct PsiSolve<'a> {
    model: &'a ArmModel,
    /// The joint limits every returned branch must satisfy, including any control
    /// floor layered over the URDF.
    limits: &'a [Limit; ARM_DOF],
    r_d: Rotation3<f64>,
    p_w: Vector3<f64>,
    theta4: f64,
    circle: Circle,
}

impl PsiSolve<'_> {
    /// The arm angle within `max_step_rad` of `psi0` with the highest
    /// manipulability, and its joint solution. Evaluates the window's endpoints
    /// and center, refines once with a parabolic fit through them, and keeps the
    /// best solvable candidate; `psi0` itself is always a candidate, so this
    /// returns `None` only if `psi0` has no in-limit branch (which the caller's
    /// feasibility selection rules out).
    fn max_manipulability_step(
        &self,
        psi0: f64,
        max_step_rad: f64,
        seed: &JointVec,
    ) -> Option<(f64, JointVec)> {
        // A non-finite or negative step degenerates to FromSeed rather than
        // poisoning the candidate angles.
        let h = if max_step_rad.is_finite() {
            max_step_rad.max(0.0)
        } else {
            0.0
        };
        let eval = |psi: f64| {
            self.solve_at_psi(psi, seed)
                .map(|q| (psi, manipulability(&jacobian_poe(self.model, &q)), q))
        };
        let center = eval(psi0)?;
        let below = eval(psi0 - h);
        let above = eval(psi0 + h);
        // With all three samples, a concave parabola through them estimates the
        // peak; clamped into the window so the step bound holds regardless of fit
        // quality.
        let vertex = match (&below, &above) {
            (Some((_, w_lo, _)), Some((_, w_hi, _))) => {
                let curvature = w_lo - 2.0 * center.1 + w_hi;
                if curvature < 0.0 {
                    let offset = 0.5 * h * (w_lo - w_hi) / curvature;
                    eval(psi0 + offset.clamp(-h, h))
                } else {
                    None
                }
            }
            _ => None,
        };
        // Strict improvement over the center keeps the chain anchored on plateaus
        // (no drift when manipulability is locally flat).
        let best = [below, above, vertex]
            .into_iter()
            .flatten()
            .fold(center, |acc, c| if c.1 > acc.1 { c } else { acc });
        Some((best.0, best.2))
    }
}

/// Geometric Jacobian of configuration `q` from the PoE screw data: column `i` is
/// `[z x (p_ee - p); z]` with joint `i`'s axis `z` and axis point `p` carried
/// through joints `1..i-1`, matching [`Posed::jacobian`]'s layout (rows 0..3
/// linear, 3..6 angular, arm base frame, EE origin reference; the two agree, see
/// tests). Lets the solver score manipulability without the FK chain's `&mut`.
///
/// [`Posed::jacobian`]: crate::fk::Posed::jacobian
fn jacobian_poe(model: &ArmModel, q: &JointVec) -> Jacobian {
    let p_ee = fk_poe_position(model, q);
    // Accumulate the affine map of joints 1..i-1 (p -> r_acc p + t_acc); joint i's
    // current axis and axis point are its home screw carried through it.
    let mut r_acc = Rotation3::identity();
    let mut t_acc = Vector3::zeros();
    let mut cols = [Vector6::zeros(); ARM_DOF];
    for i in 0..ARM_DOF {
        let z = r_acc * model.axes[i];
        let p = r_acc * model.points[i] + t_acc;
        let linear = z.cross(&(p_ee - p));
        cols[i] = Vector6::new(linear.x, linear.y, linear.z, z.x, z.y, z.z);
        let r_i = exp_so3(model.axes[i], q[i]);
        t_acc += r_acc * (model.points[i] - r_i * model.points[i]);
        r_acc *= r_i;
    }
    Jacobian::from_columns(&cols)
}

impl PsiSolve<'_> {
    /// The shoulder rotation `R_s(psi)` (home upper-arm frame to the target one)
    /// and the residual wrist rotation `R_w(psi)` left to invert, for one arm
    /// angle. Both are smooth functions of `psi`:
    /// [`solve_at_psi`](Self::solve_at_psi) decomposes them into joint angles, and
    /// [`feasible_psi_intervals`](Self::feasible_psi_intervals) samples them to
    /// recover the per-joint `psi` dependence in closed form. `None` at the
    /// straight-arm pose, where the arm plane (and so `R_s`) is undefined.
    fn shoulder_wrist_rotations(&self, psi: f64) -> Option<(Rotation3<f64>, Rotation3<f64>)> {
        let PsiSolve {
            model,
            r_d,
            p_w,
            theta4,
            circle,
            ..
        } = self;
        let elbow = circle.elbow(psi);

        // Upper-arm / forearm directions and the arm-plane normal.
        let e_t = (elbow - model.shoulder) / model.l_su;
        let f_t = (*p_w - elbow) / model.l_uw;
        let n_plane = e_t.cross(&f_t); // |.| = sin(angle between upper arm and forearm)
        if n_plane.norm() < PARALLEL_SIN_EPS {
            return None; // straight arm: arm plane undefined
        }
        let n_plane = n_plane.normalize();

        // Shoulder rotation R_s maps the home upper-arm frame to the target one.
        let u0 = (model.elbow_home - model.shoulder) / model.l_su;
        let a4 = model.axes[3];
        let r_s = frame_map(u0, a4, e_t, n_plane);

        // Rotation through joint 4, then the residual wrist rotation to invert.
        let r_upto4 = r_s * exp_so3(a4, *theta4);
        let r_home = model.home_tip.rotation.to_rotation_matrix();
        let r_w = r_upto4.inverse() * r_d * r_home.inverse();
        Some((r_s, r_w))
    }

    /// The seed's elbow azimuth measured on **this target's** redundancy circle,
    /// so it is directly comparable with
    /// [`feasible_psi_intervals`](Self::feasible_psi_intervals) and with the psi
    /// that [`solve_at_psi`](Self::solve_at_psi) builds from. Measuring it on the
    /// seed's own circle instead compares two angles taken against different
    /// references, and the two circles' frames diverge as the seed's wrist center
    /// moves away from the target's.
    ///
    /// `None` when the seed's elbow projects onto this circle's axis, where there
    /// is no direction to preserve.
    fn preferred_psi(&self, seed: &JointVec) -> Option<f64> {
        self.circle.azimuth(elbow_position(self.model, seed))
    }

    /// Best in-limit joint solution for a fixed arm angle `psi`, or `None` if no
    /// branch is in limits. Reach and `theta4` are already established by
    /// [`solve`].
    fn solve_at_psi(&self, psi: f64, seed: &JointVec) -> Option<JointVec> {
        let model = self.model;
        let (r_s, r_w) = self.shoulder_wrist_rotations(psi)?;

        let shoulders = decompose_three_axes(&r_s, model.axes[0], model.axes[1], model.axes[2]);
        let wrists = decompose_three_axes(&r_w, model.axes[4], model.axes[5], model.axes[6]);

        // Keep the in-limits branch nearest the seed. Each joint is normalized
        // into its limit window so the returned value itself respects the
        // declared limits (not merely a 2π-equivalent of it).
        let mut best: Option<(f64, JointVec)> = None;
        for &(t1, t2, t3) in &shoulders {
            for &(t5, t6, t7) in &wrists {
                let Some(q) =
                    normalize_into_limits(self.limits, &[t1, t2, t3, self.theta4, t5, t6, t7])
                else {
                    continue;
                };
                let cost = seed_distance(&q, seed);
                if best.as_ref().is_none_or(|(c, _)| cost < *c) {
                    best = Some((cost, q));
                }
            }
        }
        best.map(|(_, q)| q)
    }
}

// ---------------------------------------------------------------------------
// Analytic feasible arm-angle (psi) intervals.
//
// With the wrist center, theta4 and the discrete branch fixed, the shoulder and
// the wrist are each a spherical joint (three orthonormal intersecting axes)
// whose home rotation is R_s(psi) / R_w(psi). Conjugating that rotation into its
// own axis frame B = [w1 w2 w3] gives a standard XYZ-Euler matrix
//   M(psi) = Bᵀ R(psi) B = Rx(σθ1) Ry(σθ2) Rz(σθ3),   σ = det(B) = ±1,
// so the joint angles read off entries of M: the two "pivot" joints (θ1, θ3)
// from atan2 of two entries, the middle "hinge" joint (θ2) from one. Crucially
// every entry of M(psi) is a pure sinusoid `a sin psi + b cos psi + c` (rotating
// by psi about the fixed S-W axis acts linearly in (sin psi, cos psi)), so a
// joint reaching a limit L is the root set of `a sin psi + b cos psi = c`, in
// closed form. The union of those roots over all six joints and both limits is a
// superset of every feasibility boundary; classifying the arcs between them (one
// feasibility test per arc) yields the exact feasible set. This is the Shimizu /
// Elias-Wen feasible-arm-angle computation cited in the README, specialized to
// the model's PoE axes. See `tests/psi_completeness.rs` for the end-to-end
// guarantee this buys, and `ik::tests::feasible_intervals_match_brute_force` for
// the check that the analytic set equals a fine-grained feasibility scan.

/// Step taken just inside a feasible arc when the nearest feasible arm angle
/// lands on an arc boundary (radians), so the exact-limit endpoint clears the
/// inclusive joint-limit check. Capped at the arc half-width in
/// [`nearest_feasible_psi`], so it always lands strictly inside a feasible arc
/// rather than overshooting a thin one.
const PSI_BOUNDARY_NUDGE: f64 = 1e-6;

/// All `psi in (-pi, pi]` solving `a sin psi + b cos psi = c`.
fn sincos_roots(a: f64, b: f64, c: f64) -> Vec<f64> {
    // a sin psi + b cos psi = r cos(psi - phi), with r = hypot(a, b).
    let r = a.hypot(b);
    if r < ALGEBRAIC_EPS {
        return Vec::new(); // no psi dependence: this limit yields no boundary
    }
    let ratio = c / r;
    if ratio.abs() > 1.0 + ALGEBRAIC_EPS {
        return Vec::new(); // limit never reached on this circle
    }
    let delta = ratio.clamp(-1.0, 1.0).acos();
    let phi = a.atan2(b);
    if delta < ALGEBRAIC_EPS {
        vec![wrap_pi(phi)]
    } else {
        vec![wrap_pi(phi + delta), wrap_pi(phi - delta)]
    }
}

/// Per-entry sinusoid coefficients `(a_sin, b_cos, c_const)` of
/// `M(psi) = Bᵀ R(psi) B`, recovered exactly from three samples (`M` is
/// sinusoidal in `psi`). `basis` carries the three joint axes as columns; the
/// samples are `R` at `psi = 0`, `+pi/2`, `-pi/2`.
fn euler_matrix_coeffs(
    basis: &Matrix3<f64>,
    r0: &Rotation3<f64>,
    r_plus: &Rotation3<f64>,
    r_minus: &Rotation3<f64>,
) -> (Matrix3<f64>, Matrix3<f64>, Matrix3<f64>) {
    let bt = basis.transpose();
    let m = |r: &Rotation3<f64>| bt * r.matrix() * basis;
    let (m0, mp, mn) = (m(r0), m(r_plus), m(r_minus));
    let a_sin = (mp - mn) * 0.5;
    let c_const = (mp + mn) * 0.5;
    let b_cos = m0 - c_const;
    (a_sin, b_cos, c_const)
}

/// Boundary `psi` values where any of a spherical limb's three joints reaches a
/// limit, appended to `events`. A superset of the true feasibility boundaries
/// (spurious extras are harmless: each arc between boundaries is classified by an
/// explicit feasibility test). `sigma = det(basis)` is the limb's handedness.
fn limb_event_angles(
    a_sin: &Matrix3<f64>,
    b_cos: &Matrix3<f64>,
    c_const: &Matrix3<f64>,
    sigma: f64,
    limits: [Limit; 3],
    events: &mut Vec<f64>,
) {
    // A "pivot" joint (θ1 from atan2(-M12, M22); θ3 from atan2(-M01, M00))
    // reaches L where  M[i] sin(σL) + M[j] cos(σL) = 0.
    {
        let mut pivot = |i: (usize, usize), j: (usize, usize), lim: Limit| {
            for l in [lim.lo, lim.hi] {
                let (s, co) = (sigma * l).sin_cos();
                let a = a_sin[i] * s + a_sin[j] * co;
                let b = b_cos[i] * s + b_cos[j] * co;
                let c = -(c_const[i] * s + c_const[j] * co);
                events.extend(sincos_roots(a, b, c));
            }
        };
        pivot((2, 2), (1, 2), limits[0]); // θ1: entries M22, M12
        pivot((0, 0), (0, 1), limits[2]); // θ3: entries M00, M01
    }
    // The "hinge" joint θ2 (sin θ2 = M02) reaches L where M02 = sin(σL).
    for l in [limits[1].lo, limits[1].hi] {
        let target = (sigma * l).sin();
        events.extend(sincos_roots(
            a_sin[(0, 2)],
            b_cos[(0, 2)],
            target - c_const[(0, 2)],
        ));
    }
}

/// The exact set of feasible arm angles for this target, as `(start, end)` arcs
/// in radians (`end` may exceed `pi` for the wrap-around arc). Empty if no arm
/// angle is feasible (including the straight-arm pose, where `R_s` is undefined
/// for every `psi`).
impl PsiSolve<'_> {
    fn feasible_psi_intervals(&self, seed: &JointVec) -> Vec<(f64, f64)> {
        let model = self.model;
        // Sample R_s / R_w at psi = 0, ±pi/2 to recover their exact sinusoidal form.
        let sample = |psi: f64| self.shoulder_wrist_rotations(psi);
        let (Some((rs0, rw0)), Some((rsp, rwp)), Some((rsn, rwn))) =
            (sample(0.0), sample(FRAC_PI_2), sample(-FRAC_PI_2))
        else {
            return Vec::new(); // straight arm: feasibility is "none" for every psi
        };

        let bs = Matrix3::from_columns(&[model.axes[0], model.axes[1], model.axes[2]]);
        let bw = Matrix3::from_columns(&[model.axes[4], model.axes[5], model.axes[6]]);
        let (sa_s, cb_s, cc_s) = euler_matrix_coeffs(&bs, &rs0, &rsp, &rsn);
        let (sa_w, cb_w, cc_w) = euler_matrix_coeffs(&bw, &rw0, &rwp, &rwn);

        let mut events = Vec::new();
        let shoulder_limits = [self.limits[0], self.limits[1], self.limits[2]];
        let wrist_limits = [self.limits[4], self.limits[5], self.limits[6]];
        limb_event_angles(
            &sa_s,
            &cb_s,
            &cc_s,
            bs.determinant(),
            shoulder_limits,
            &mut events,
        );
        limb_event_angles(
            &sa_w,
            &cb_w,
            &cc_w,
            bw.determinant(),
            wrist_limits,
            &mut events,
        );

        intervals_from_events(events, |psi| self.solve_at_psi(psi, seed).is_some())
    }
}

/// Partition the circle at `events` and keep the arcs whose interior is feasible.
/// Events need no de-duplication: coincident ones only yield zero-width arcs,
/// which are harmless (their would-be interior collapses to a single, already
/// feasibility-classified point).
fn intervals_from_events(mut events: Vec<f64>, feasible: impl Fn(f64) -> bool) -> Vec<(f64, f64)> {
    events.sort_by(|a, b| a.partial_cmp(b).expect("psi roots are finite"));
    if events.is_empty() {
        // Feasibility is constant over the whole circle.
        return if feasible(0.0) {
            vec![(-PI, PI)]
        } else {
            Vec::new()
        };
    }
    let n = events.len();
    let mut intervals = Vec::new();
    for k in 0..n {
        let start = events[k];
        let end = if k + 1 < n {
            events[k + 1]
        } else {
            events[0] + TAU
        };
        if feasible(wrap_pi(0.5 * (start + end))) {
            intervals.push((start, end));
        }
    }
    intervals
}

/// The feasible arm angle closest (on the circle) to `preferred`, or `None` if
/// no arm angle is feasible. When the nearest point is an arc boundary it is
/// nudged just inside the arc so the returned angle is strictly feasible.
fn nearest_feasible_psi(intervals: &[(f64, f64)], preferred: f64) -> Option<f64> {
    let mut best: Option<(f64, f64)> = None; // (circular distance, psi)
    for &(start, end) in intervals {
        // Bring `preferred` into [start, start + 2pi) to test arc membership.
        let p = start + (preferred - start).rem_euclid(TAU);
        let psi = if p <= end {
            p // preferred itself is feasible
        } else {
            // Nearest endpoint, nudged just inside the (possibly thin) arc.
            let nudge = (0.5 * (end - start)).min(PSI_BOUNDARY_NUDGE);
            if wrap_pi(end - preferred).abs() <= wrap_pi(start - preferred).abs() {
                end - nudge
            } else {
                start + nudge
            }
        };
        let dist = wrap_pi(psi - preferred).abs();
        if best.is_none_or(|(d, _)| dist < d) {
            best = Some((dist, psi));
        }
    }
    best.map(|(_, psi)| wrap_pi(psi))
}

/// Rotation taking the orthonormal frame `[x0, y0, x0×y0]` to
/// `[x1, y1, x1×y1]`. `x0⊥y0` and `x1⊥y1` are required (asserted by callers'
/// geometry).
fn frame_map(
    x0: Vector3<f64>,
    y0: Vector3<f64>,
    x1: Vector3<f64>,
    y1: Vector3<f64>,
) -> Rotation3<f64> {
    let h = Matrix3::from_columns(&[x0, y0, x0.cross(&y0)]);
    let t = Matrix3::from_columns(&[x1, y1, x1.cross(&y1)]);
    Rotation3::from_matrix_unchecked(t * h.transpose())
}

/// Normalize each joint to its 2π-equivalent inside the limit window and return
/// the wrapped configuration, or `None` if any joint has no in-limit
/// representative. Returning the wrapped value (not the raw atan2 output)
/// guarantees the solution itself respects the declared limits.
fn normalize_into_limits(limits: &[Limit; ARM_DOF], q: &JointVec) -> Option<JointVec> {
    let mut out = [0.0; ARM_DOF];
    for (slot, (&v, l)) in out.iter_mut().zip(q.iter().zip(limits)) {
        let w = wrap_into(v, l.lo, l.hi);
        if !l.contains(w) {
            return None;
        }
        *slot = w;
    }
    Some(out)
}

/// Distance to the seed, comparing each joint at the representative closest to
/// the seed (so a `+2π` alias doesn't look far).
fn seed_distance(q: &JointVec, seed: &JointVec) -> f64 {
    q.iter()
        .zip(seed)
        .map(|(&v, &s)| {
            let d = wrap_pi(v - s);
            d * d
        })
        .sum()
}

/// Wrap `x` toward `[lo, hi]` by multiples of `2π` (joint angles are modular);
/// returns the representative nearest the interval midpoint. For any window
/// narrower than `2π` (all OpenArm joints) this is the unique in-limit alias
/// when one exists.
fn wrap_into(x: f64, lo: f64, hi: f64) -> f64 {
    let mid = 0.5 * (lo + hi);
    mid + wrap_pi(x - mid)
}

/// Wrap an angle to the half-open principal range `[-π, π)`.
fn wrap_pi(x: f64) -> f64 {
    (x + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU) - std::f64::consts::PI
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fk::ForwardKinematics;
    use crate::test_support::{v1_fk, v1_limits, v1_model};
    use nalgebra::{Isometry3, UnitQuaternion};
    use rand::RngExt;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    /// Uniform in-limit joint sample. Tests seed a `StdRng` with a fixed
    /// constant, so the spread is reproducible (no flaky randomness).
    fn sample_q(rng: &mut StdRng, limits: &[Limit; ARM_DOF]) -> JointVec {
        std::array::from_fn(|i| {
            let l = limits[i];
            rng.random_range(l.lo..l.hi)
        })
    }

    /// EE pose at `q` (thin wrapper; `ee_pose` already poses the chain).
    fn pose(fk: &mut ForwardKinematics, q: &JointVec) -> Isometry3<f64> {
        fk.at(q).ee_pose()
    }

    /// Assemble an EE target pose from a rotation + position (for the tests that
    /// build a target directly rather than from FK).
    fn iso(r: Rotation3<f64>, p: Vector3<f64>) -> Isometry3<f64> {
        Isometry3::from_parts(p.into(), UnitQuaternion::from_rotation_matrix(&r))
    }

    #[test]
    fn round_trip_random_samples() {
        let mut fk = v1_fk("left");
        let lim = fk.limits();
        let m = ArmModel::from_fk(&mut fk).unwrap();
        let mut rng = StdRng::seed_from_u64(0xC0FFEE);

        let n = 5000;
        let (mut ok, mut singular, mut fail) = (0, 0, 0);
        for _ in 0..n {
            let q = sample_q(&mut rng, &lim);
            // Skip near the straight-arm boundary (expected singular misses).
            if q[3] < 0.05 {
                singular += 1;
                continue;
            }
            let target = pose(&mut fk, &q);
            let p_d = target.translation.vector;

            let Some(sol) = solve(&m, &lim, &target, ArmAnglePolicy::FromSeed, &q) else {
                fail += 1;
                continue;
            };
            // Every returned joint must respect its declared limit, not just a
            // 2π-equivalent of it.
            for (i, (&v, l)) in sol.q.iter().zip(&lim).enumerate() {
                assert!(
                    l.contains(v),
                    "joint {i} = {v} outside [{}, {}]",
                    l.lo,
                    l.hi
                );
            }
            let got = pose(&mut fk, &sol.q);
            let pos_err = (got.translation.vector - p_d).norm();
            let rot_err = got.rotation.angle_to(&target.rotation);
            if pos_err < 1e-6 && rot_err < 1e-6 {
                ok += 1;
            } else {
                fail += 1;
            }
        }
        let solved = n - singular;
        println!("round-trip: {ok}/{solved} ok, {fail} fail, {singular} skipped(singular)");
        // Non-singular failures indicate a branch-sign or frame bug.
        assert!(
            fail as f64 / (solved as f64) < 0.01,
            "{fail}/{solved} non-singular failures"
        );
    }

    #[test]
    fn self_consistency_recovers_seed() {
        let mut fk = v1_fk("left");
        let lim = fk.limits();
        let m = ArmModel::from_fk(&mut fk).unwrap();
        let mut rng = StdRng::seed_from_u64(7);
        for _ in 0..500 {
            let q = sample_q(&mut rng, &lim);
            if q[3] < 0.1 {
                continue;
            }
            let target = pose(&mut fk, &q);
            let psi = arm_angle_of(&m, &q).expect("q is non-singular (q[3] >= 0.1)");
            let sol = solve(&m, &lim, &target, ArmAnglePolicy::Fixed(psi), &q)
                .expect("exact psi should solve");
            // Same branch as seed: each joint within a small tolerance.
            for (i, (&got, &want)) in sol.q.iter().zip(&q).enumerate() {
                assert!(
                    wrap_pi(got - want).abs() < 1e-5,
                    "joint {i}: got {got} want {want}"
                );
            }
        }
    }

    #[test]
    fn right_arm_round_trip() {
        let mut fk = v1_fk("right");
        let lim = fk.limits();
        let m = ArmModel::from_fk(&mut fk).unwrap();
        let mut rng = StdRng::seed_from_u64(99);
        let mut checked = 0;
        for _ in 0..1000 {
            let q = sample_q(&mut rng, &lim);
            if q[3] < 0.1 {
                continue;
            }
            let target = pose(&mut fk, &q);
            let sol = solve(&m, &lim, &target, ArmAnglePolicy::FromSeed, &q)
                .expect("right-arm target should solve");
            let got = pose(&mut fk, &sol.q);
            assert!((got.translation.vector - target.translation.vector).norm() < 1e-6);
            assert!(got.rotation.angle_to(&target.rotation) < 1e-6);
            checked += 1;
        }
        assert!(
            checked > 800,
            "too few right-arm samples checked: {checked}"
        );
    }

    #[test]
    fn arm_angle_sweep_holds_pose() {
        // The whole redundancy circle maps to the same EE pose: solving at many
        // arm angles (not the seed's) must still close the round-trip. This is
        // the non-circular check that psi is a true redundancy parameter.
        let mut fk = v1_fk("left");
        let lim = fk.limits();
        let m = ArmModel::from_fk(&mut fk).unwrap();
        let mut rng = StdRng::seed_from_u64(2024);
        let q = loop {
            let q = sample_q(&mut rng, &lim);
            if q[3] > 0.5 {
                break q;
            }
        };
        let target = pose(&mut fk, &q);
        let p_d = target.translation.vector;

        let mut feasible = 0;
        for k in 0..72 {
            let psi = -std::f64::consts::PI + k as f64 * std::f64::consts::TAU / 72.0;
            if let Some(sol) = solve(&m, &lim, &target, ArmAnglePolicy::Fixed(psi), &q) {
                let got = pose(&mut fk, &sol.q);
                assert!(
                    (got.translation.vector - p_d).norm() < 1e-6
                        && got.rotation.angle_to(&target.rotation) < 1e-6,
                    "psi={psi}: pose drift",
                );
                feasible += 1;
            }
        }
        // The seed's own arm angle is feasible, so at least a contiguous arc is.
        assert!(feasible > 0, "no feasible arm angle found");
    }

    #[test]
    fn cartesian_trajectory_servos_continuously() {
        // Trace a smooth closed Cartesian path (a circle in position plus a gentle
        // orientation wobble) and servo along it in FromSeed mode, seeding each
        // solve with the *previous* solution, as a real servo consumer would.
        // This is the one test that exercises the README's continuity guarantee:
        // along a smooth path the joints move smoothly, with no branch flip or psi
        // jump, while every pose is still hit exactly.
        let mut fk = v1_fk("left");
        let lim = fk.limits();
        let m = ArmModel::from_fk(&mut fk).unwrap();

        // A comfortable mid-range posture, well off the straight-arm singularity.
        let q0 = [0.2, -0.3, 0.3, 1.0, -0.4, 0.5, 0.3];
        let home = pose(&mut fk, &q0);
        let p0 = home.translation.vector;
        let r0 = home.rotation;
        // Orthonormal basis for the position circle (base-frame X and Z).
        let (u, w) = (Vector3::x(), Vector3::z());
        let radius = 0.03; // 3 cm: stays inside reach and off the singular band

        let steps = 240;
        let mut seed = q0;
        let mut max_step = 0.0_f64;
        for k in 0..=steps {
            let t = k as f64 * std::f64::consts::TAU / steps as f64;
            let p_d = p0 + radius * (t.cos() * u + t.sin() * w);
            // Gentle wrist wobble (vanishes at t=0 and t=2π so the loop closes).
            let r_d = r0 * UnitQuaternion::from_axis_angle(&Vector3::y_axis(), 0.15 * t.sin());
            let target = Isometry3::from_parts(p_d.into(), r_d);

            let sol = solve(&m, &lim, &target, ArmAnglePolicy::FromSeed, &seed)
                .expect("trajectory point should solve");

            let got = pose(&mut fk, &sol.q);
            assert!(
                (got.translation.vector - p_d).norm() < 1e-6,
                "pos drift at step {k}"
            );
            assert!(got.rotation.angle_to(&r_d) < 1e-6, "rot drift at step {k}");

            // Continuity (the actual guarantee): the largest single-joint change
            // between steps stays small. A branch flip or psi jump would show up
            // here as a step of order 1 rad or more.
            if k > 0 {
                let step = sol
                    .q
                    .iter()
                    .zip(&seed)
                    .map(|(a, b)| wrap_pi(a - b).abs())
                    .fold(0.0, f64::max);
                assert!(
                    step < 0.05,
                    "joint jump {step:.4} at step {k}: branch flip / psi jump"
                );
                max_step = max_step.max(step);
            }
            seed = sol.q;
        }
        // A closed *task-space* loop need NOT close in joint space: the arm angle is
        // measured against a reference that rotates with the S-W direction, so the
        // wrist-center direction tracing a cone of nonzero solid angle leaves a net
        // psi offset (a geometric phase / holonomy). So this is a loose sanity bound,
        // not a tight invariant; the per-step bound above is the real guarantee.
        let closure = seed
            .iter()
            .zip(&q0)
            .map(|(a, b)| wrap_pi(a - b).abs())
            .fold(0.0, f64::max);
        println!(
            "trajectory max per-step joint change {max_step:.4} rad, loop closure {closure:.4} rad"
        );
        assert!(
            closure < 0.3,
            "loop drift {closure:.4} far larger than holonomy expects"
        );
    }

    #[test]
    fn feasible_intervals_match_brute_force() {
        // The analytic feasible-ψ set must agree with a fine brute-force scan:
        // no reachable ψ marked infeasible, and no infeasible ψ marked feasible,
        // save the single scan cell straddling an exact limit boundary. A missing
        // event (an unbracketed feasibility transition) shows up as a disagreement
        // far from any interval edge.
        let mut fk = v1_fk("left");
        let lim = fk.limits();
        let m = ArmModel::from_fk(&mut fk).unwrap();
        let mut rng = StdRng::seed_from_u64(0xA11CE);
        let seed = [0.1, -0.2, 0.15, 0.7, -0.3, 0.25, 0.2];

        const GRID: usize = 1440;
        let step = TAU / GRID as f64;

        let in_intervals = |intervals: &[(f64, f64)], psi: f64| {
            intervals
                .iter()
                .any(|&(s, e)| s + (psi - s).rem_euclid(TAU) <= e)
        };
        let edge_dist = |intervals: &[(f64, f64)], psi: f64| {
            intervals
                .iter()
                .flat_map(|&(s, e)| [wrap_pi(psi - s).abs(), wrap_pi(psi - e).abs()])
                .fold(f64::INFINITY, f64::min)
        };

        let mut targets = 0;
        for _ in 0..200 {
            let q = sample_q(&mut rng, &lim);
            if q[3] < 0.2 {
                continue; // comfortably off the straight-arm boundary
            }
            let target = pose(&mut fk, &q);
            let r_d = target.rotation.to_rotation_matrix();
            let p_w = target.translation.vector;

            let d = (p_w - m.shoulder).norm();
            let cos4 = ((d * d - m.l_su * m.l_su - m.l_uw * m.l_uw) / (2.0 * m.l_su * m.l_uw))
                .clamp(-1.0, 1.0);
            let theta4 = cos4.acos();
            let ctx = PsiSolve {
                model: &m,
                limits: &lim,
                r_d,
                p_w,
                theta4,
                circle: circle_frame(&m, p_w),
            };
            let intervals = ctx.feasible_psi_intervals(&seed);
            assert!(
                !intervals.is_empty(),
                "no feasible psi for a reachable target"
            );

            for k in 0..GRID {
                let psi = -PI + k as f64 * step;
                let brute = ctx.solve_at_psi(psi, &seed).is_some();
                let analytic = in_intervals(&intervals, psi);
                if brute != analytic {
                    assert!(
                        edge_dist(&intervals, psi) <= 1.5 * step,
                        "psi={psi:.5}: analytic={analytic} brute={brute}, {:.5} rad \
                         from any interval edge (an unbracketed feasibility transition)",
                        edge_dist(&intervals, psi),
                    );
                }
            }
            targets += 1;
        }
        assert!(targets > 100, "too few targets checked: {targets}");
    }

    #[test]
    fn poe_fk_matches_an_independent_chain() {
        // Three implementations must agree on where the end effector is: the
        // PoE screw map the IK is built on, this crate's own URDF chain, and the
        // `k` crate reading the same URDF. The third is the point - the first two
        // are ours, so on their own they could share a mistake. `k` is carried as
        // a dev-dependency for exactly this and nothing else.
        //
        // Compared component-wise rather than by subtracting `Isometry3`s, so the
        // check survives `k` and this crate resolving different `nalgebra`
        // versions.
        use k::nalgebra as kna;

        let mut fk = v1_fk("left");
        let m = ArmModel::from_fk(&mut fk).unwrap();
        let q: JointVec = [0.3, -0.4, 0.2, 0.8, -0.5, 0.3, 0.6];

        let ours = pose(&mut fk, &q);
        let p_poe = fk_poe_position(&m, &q);
        assert!(
            (ours.translation.vector - p_poe).norm() < 1e-9,
            "PoE and chain FK disagree on position"
        );
        let r_poe = fk_poe_rotation(&m, &q);
        assert!(
            (r_poe.matrix() - ours.rotation.to_rotation_matrix().matrix()).norm() < 1e-9,
            "PoE and chain FK disagree on orientation"
        );

        let robot =
            urdf_rs::read_from_string(crate::test_support::FIXTURE_URDF).expect("parse fixture");
        let chain = k::Chain::<f64>::from(&robot);
        let tip = chain
            .find_link("openarm_left_link7")
            .expect("fixture has link7");
        let serial = k::SerialChain::from_end(tip);
        serial.set_joint_positions_clamped(&q);
        serial.update_transforms();
        let base = chain
            .find_link("openarm_left_link0")
            .expect("fixture has link0")
            .world_transform()
            .expect("base world transform");
        let theirs = base.inverse() * tip.world_transform().expect("tip world transform");

        let dp = [
            theirs.translation.vector.x - ours.translation.vector.x,
            theirs.translation.vector.y - ours.translation.vector.y,
            theirs.translation.vector.z - ours.translation.vector.z,
        ];
        assert!(
            dp.iter().all(|d| d.abs() < 1e-9),
            "independent chain disagrees on position by {dp:?}"
        );
        let theirs_r: kna::Matrix3<f64> = *theirs.rotation.to_rotation_matrix().matrix();
        let ours_r = ours.rotation.to_rotation_matrix();
        for r in 0..3 {
            for c in 0..3 {
                let d = theirs_r[(r, c)] - ours_r.matrix()[(r, c)];
                assert!(
                    d.abs() < 1e-9,
                    "independent chain disagrees on orientation at ({r},{c}) by {d}"
                );
            }
        }
    }

    // --- Direct unit tests for the pure helpers --------------------------

    #[test]
    fn wrap_pi_maps_into_principal_range() {
        use std::f64::consts::{PI, TAU};
        assert!((wrap_pi(0.0) - 0.0).abs() < 1e-12);
        assert!((wrap_pi(3.0) - 3.0).abs() < 1e-12);
        assert!((wrap_pi(3.0 + TAU) - 3.0).abs() < 1e-12);
        assert!((wrap_pi(-3.0 - TAU) + 3.0).abs() < 1e-12);
        // Result is always within [-π, π).
        for k in -20..20 {
            let w = wrap_pi(0.3 + k as f64 * TAU);
            assert!(w > -PI - 1e-9 && w <= PI + 1e-9, "out of range: {w}");
        }
    }

    #[test]
    fn wrap_into_finds_in_window_alias() {
        // Symmetric window: identity for in-window values.
        assert!((wrap_into(0.5, -1.0, 1.0) - 0.5).abs() < 1e-12);
        // A value 2π below the window maps up into it.
        assert!((wrap_into(0.5 - std::f64::consts::TAU, -1.0, 1.0) - 0.5).abs() < 1e-12);
        // Asymmetric j1-like window: a near-(-π) value has its +2π alias in range.
        let (lo, hi) = (-1.396, 3.49);
        let w = wrap_into(-3.0, lo, hi);
        assert!((w - (-3.0 + std::f64::consts::TAU)).abs() < 1e-9 && (lo..=hi).contains(&w));
        // One-sided j4 window keeps an in-range value put.
        assert!((wrap_into(1.0, 0.0, 2.443) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn normalize_into_limits_wraps_or_rejects() {
        let lim = v1_limits("left");
        // A raw j1 = -3.0 is outside [-1.396, 3.49] but its +2π alias is in.
        let raw = [-3.0, 0.0, 0.0, 0.5, 0.0, 0.0, 0.0];
        let out = normalize_into_limits(&lim, &raw).expect("alias is in range");
        for (v, l) in out.iter().zip(&lim) {
            assert!(l.contains(*v), "{v} not in [{},{}]", l.lo, l.hi);
        }
        // A value with no in-window alias (j6 window is narrow) is rejected.
        let bad = [0.0, 0.0, 0.0, 0.5, 0.0, std::f64::consts::PI, 0.0];
        assert!(normalize_into_limits(&lim, &bad).is_none());
    }

    #[test]
    fn subproblem1_recovers_rotation_angle() {
        let axis = Vector3::z();
        let p = Vector3::new(1.0, 0.0, 0.3); // off-axis component rotates
        for &theta in &[0.0, 0.5, -1.2, 3.0] {
            let q = exp_so3(axis, theta) * p;
            assert!((wrap_pi(subproblem1(axis, p, q) - theta)).abs() < 1e-9);
        }
    }

    #[test]
    fn subproblem2_solves_and_rejects() {
        let w1 = Vector3::z();
        let w2 = Vector3::x();
        let p = Vector3::new(0.4, 0.5, 0.6);
        let (a, b) = (0.7, -0.9);
        let q = exp_so3(w1, a) * exp_so3(w2, b) * p;
        let sols = subproblem2(w1, w2, p, q);
        assert!(!sols.is_empty());
        // At least one branch reproduces q.
        assert!(sols.iter().any(|&(t1, t2)| {
            let got = exp_so3(w1, t1) * exp_so3(w2, t2) * p;
            (got - q).norm() < 1e-9
        }));
        // Parallel axes -> no solution.
        assert!(subproblem2(w1, w1, p, q).is_empty());
        // Geometrically inconsistent target -> gamma_sq < 0 -> no solution.
        // Rotation about x can't change p's x-component (0.4), so a same-norm
        // pure-+z target is unreachable.
        let unreachable = Vector3::z() * p.norm();
        assert!(subproblem2(w1, w2, p, unreachable).is_empty());
    }

    #[test]
    fn decompose_three_axes_reconstructs_rotation() {
        let (w1, w2, w3) = (Vector3::z(), Vector3::x(), Vector3::y());
        let r = exp_so3(w1, 0.6) * exp_so3(w2, -0.4) * exp_so3(w3, 1.1);
        let sols = decompose_three_axes(&r, w1, w2, w3);
        assert!(!sols.is_empty());
        assert!(sols.iter().any(|&(t1, t2, t3)| {
            let got = exp_so3(w1, t1) * exp_so3(w2, t2) * exp_so3(w3, t3);
            (got.matrix() - r.matrix()).norm() < 1e-9
        }));
    }

    #[test]
    fn frame_map_sends_home_frame_to_target() {
        let x0 = Vector3::y();
        let y0 = Vector3::z();
        let x1 = Vector3::x();
        let y1 = Vector3::y();
        let r = frame_map(x0, y0, x1, y1);
        assert!((r * x0 - x1).norm() < 1e-9);
        assert!((r * y0 - y1).norm() < 1e-9);
    }

    #[test]
    fn unreachable_target_returns_none() {
        let m = v1_model("left");
        let lim = v1_limits("left");
        // Far beyond max reach (l_su + l_uw = 0.436 from the shoulder).
        let p_d = m.shoulder + Vector3::new(1.0, 0.0, 0.0);
        let target = iso(Rotation3::identity(), p_d);
        assert!(solve(&m, &lim, &target, ArmAnglePolicy::FromSeed, &[0.0; ARM_DOF]).is_none());
    }

    #[test]
    fn infeasible_arm_angle_returns_none() {
        let mut fk = v1_fk("left");
        let lim = fk.limits();
        let m = ArmModel::from_fk(&mut fk).unwrap();
        let q = [0.2, -0.3, 0.1, 0.9, 0.2, -0.1, 0.3];
        let target = pose(&mut fk, &q);
        // Sweep arm angle; an out-of-band psi must drive every joint branch out
        // of limits for at least some value, yielding None there.
        let any_none = (0..360).any(|d| {
            let psi = (d as f64).to_radians();
            solve(&m, &lim, &target, ArmAnglePolicy::Fixed(psi), &q).is_none()
        });
        assert!(any_none, "expected some arm angle to be infeasible");
    }

    #[test]
    fn near_singular_seed_still_solves() {
        // A seed at the singular home pose (q4 = 0) carries no usable arm angle,
        // so FromSeed must resolve from the feasible set and still recover a
        // solution for reachable targets rather than returning None.
        let mut fk = v1_fk("left");
        let lim = fk.limits();
        let m = ArmModel::from_fk(&mut fk).unwrap();
        let mut rng = StdRng::seed_from_u64(11);
        let home = [0.0; ARM_DOF]; // q4 = 0: the singular seed
        let (mut ok, mut total) = (0, 0);
        for _ in 0..1500 {
            let q = sample_q(&mut rng, &lim);
            if q[3] < 0.2 {
                continue; // keep targets comfortably off the straight-arm boundary
            }
            total += 1;
            let target = pose(&mut fk, &q);
            if let Some(sol) = solve(&m, &lim, &target, ArmAnglePolicy::FromSeed, &home) {
                let got = pose(&mut fk, &sol.q);
                assert!((got.translation.vector - target.translation.vector).norm() < 1e-6);
                assert!(got.rotation.angle_to(&target.rotation) < 1e-6);
                ok += 1;
            }
        }
        let rate = ok as f64 / total as f64;
        println!("singular-seed solve rate: {ok}/{total} = {rate:.3}");
        assert!(rate > 0.98, "singular seed solved only {ok}/{total}");
    }

    #[test]
    fn near_straight_seed_reproduces_its_own_configuration() {
        // A near-straight seed (e.g. the hanging home pose at q4 ≈ 0.05) is not at
        // the singularity: its elbow-circle radius is millimetres, not zero, so its
        // arm angle is well-defined. FromSeed must return the seed itself when
        // solving for the seed's own pose, rather than reconfiguring in place to a
        // different feasible arm angle for the same end-effector pose.
        let mut fk = v1_fk("left");
        let lim = fk.limits();
        let m = ArmModel::from_fk(&mut fk).unwrap();
        let seeds = [
            [0.0, 0.0, 0.0, 0.05, 0.0, 0.0, 0.0],   // hanging home
            [0.3, -0.2, 0.1, 0.03, 0.4, 0.1, -0.2], // near-straight, off-axis
        ];
        for seed in seeds {
            // These are near-straight but not singular: the arm angle is defined.
            assert!(
                arm_angle_of(&m, &seed).is_some(),
                "near-straight seed should still have a defined arm angle: {seed:?}"
            );
            let target = pose(&mut fk, &seed);
            let sol = solve(&m, &lim, &target, ArmAnglePolicy::FromSeed, &seed)
                .expect("the seed's own pose is solvable");
            for (i, (&got, &want)) in sol.q.iter().zip(&seed).enumerate() {
                assert!(
                    wrap_pi(got - want).abs() < 1e-6,
                    "joint {i}: got {got}, want {want} — seed not reproduced (arm-angle jump?)"
                );
            }
        }
    }

    #[test]
    fn cartesian_departure_from_near_straight_start_is_continuous() {
        // Walking the EE from a near-straight hanging start to a bent reach pose,
        // seeding each solve from the previous solution, must keep every joint step
        // small: the solution tracks the seed continuously, with no instantaneous
        // reconfiguration as the arm leaves the near-singular region.
        let mut fk = v1_fk("left");
        let lim = fk.limits();
        let m = ArmModel::from_fk(&mut fk).unwrap();
        let start_q = [0.0, 0.0, 0.0, 0.05, 0.0, 0.0, 0.0]; // hanging
        let reach_q = [0.0, -0.3, 0.0, 1.2, 0.0, 0.4, 0.0]; // bent reach
        let start = pose(&mut fk, &start_q);
        let end = pose(&mut fk, &reach_q);

        let n = 500; // ~5 s move at 100 Hz
        let mut seed = start_q;
        let mut prev = start_q;
        let mut worst = 0.0_f64;
        for k in 0..=n {
            let s = k as f64 / n as f64;
            let p = start.translation.vector.lerp(&end.translation.vector, s);
            let r = start
                .rotation
                .try_slerp(&end.rotation, s, 1e-6)
                .unwrap_or(end.rotation);
            let target = Isometry3::from_parts(p.into(), r);
            let sol = solve(&m, &lim, &target, ArmAnglePolicy::FromSeed, &seed)
                .expect("departure path stays reachable");
            let step = sol
                .q
                .iter()
                .zip(&prev)
                .map(|(&a, &b)| wrap_pi(a - b).abs())
                .fold(0.0, f64::max);
            worst = worst.max(step);
            prev = sol.q;
            seed = sol.q;
        }
        println!("worst joint step over departure: {worst:.4} rad");
        assert!(
            worst < 0.1,
            "largest joint step {worst} rad — discontinuous departure"
        );
    }

    #[test]
    fn rejects_non_finite_target() {
        let m = v1_model("left");
        let lim = v1_limits("left");
        let r = Rotation3::identity();
        let seed = [0.0; ARM_DOF];
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(
                solve(
                    &m,
                    &lim,
                    &iso(r, Vector3::new(bad, 0.0, 0.2)),
                    ArmAnglePolicy::FromSeed,
                    &seed
                )
                .is_none()
            );
            assert!(
                solve(
                    &m,
                    &lim,
                    &iso(r, Vector3::new(0.1, bad, 0.2)),
                    ArmAnglePolicy::Fixed(0.0),
                    &seed
                )
                .is_none()
            );
        }
    }

    #[test]
    fn branch_selection_follows_seed() {
        // The discrete shoulder/wrist branch is chosen nearest the seed: seeding
        // with a given branch must return that branch. NOTE: the OpenArm's real
        // joint limits prune every target to a single in-limit branch, so the
        // tie-break is never exercised in production; we widen the limits here to
        // expose the alternate branches and verify the selection logic.
        let mut fk = v1_fk("left");
        let m = ArmModel::from_fk(&mut fk).unwrap();
        let mut lim = fk.limits();
        for l in &mut lim {
            l.lo = -3.2;
            l.hi = 3.2;
        }
        let mut rng = StdRng::seed_from_u64(123);
        let same =
            |a: &JointVec, b: &JointVec| a.iter().zip(b).all(|(x, y)| wrap_pi(x - y).abs() < 1e-4);
        for _ in 0..300 {
            let q = sample_q(&mut rng, &lim);
            if q[3] < 0.3 {
                continue;
            }
            let target = pose(&mut fk, &q);
            let psi = arm_angle_of(&m, &q).expect("q is non-singular (q[3] >= 0.1)");
            // Collect distinct in-limit branches at this fixed arm angle.
            let mut branches: Vec<JointVec> = Vec::new();
            for _ in 0..50 {
                let s = sample_q(&mut rng, &lim);
                if let Some(sol) = solve(&m, &lim, &target, ArmAnglePolicy::Fixed(psi), &s)
                    && !branches.iter().any(|e| same(e, &sol.q))
                {
                    branches.push(sol.q);
                }
            }
            if branches.len() < 2 {
                continue;
            }
            // Seeding with each distinct branch returns that branch.
            for b in &branches {
                let got = solve(&m, &lim, &target, ArmAnglePolicy::Fixed(psi), b)
                    .unwrap()
                    .q;
                assert!(same(b, &got), "seed branch not returned: {b:?} -> {got:?}");
            }
            return; // verified on the first multi-branch target
        }
        panic!("no target with >=2 distinct branches found");
    }

    #[test]
    fn poe_jacobian_matches_chain_jacobian() {
        // The PoE Jacobian scoring manipulability must agree with the k-chain
        // geometric Jacobian entrywise, for both mirror arms.
        let mut rng = StdRng::seed_from_u64(11);
        for side in ["left", "right"] {
            let mut fk = v1_fk(side);
            let lim = fk.limits();
            let m = ArmModel::from_fk(&mut fk).unwrap();
            for _ in 0..25 {
                let q = sample_q(&mut rng, &lim);
                let poe = jacobian_poe(&m, &q);
                let chain = fk.at(&q).jacobian();
                let err = (poe - chain).abs().max();
                assert!(err < 1e-9, "{side}: Jacobians disagree by {err}");
            }
        }
    }

    #[test]
    fn max_manipulability_zero_step_matches_from_seed() {
        let mut rng = StdRng::seed_from_u64(12);
        let mut fk = v1_fk("left");
        let lim = fk.limits();
        let m = ArmModel::from_fk(&mut fk).unwrap();
        for _ in 0..25 {
            let q = sample_q(&mut rng, &lim);
            let target = pose(&mut fk, &q);
            let seed = sample_q(&mut rng, &lim);
            let from_seed = solve(&m, &lim, &target, ArmAnglePolicy::FromSeed, &seed);
            let zero_step = solve(
                &m,
                &lim,
                &target,
                ArmAnglePolicy::MaxManipulability { max_step_rad: 0.0 },
                &seed,
            );
            match (from_seed, zero_step) {
                (None, None) => {}
                (Some(a), Some(b)) => {
                    assert!((0..ARM_DOF).all(|i| (a.q[i] - b.q[i]).abs() < 1e-9));
                }
                (a, b) => panic!("solvability diverged: {a:?} vs {b:?}"),
            }
        }
    }

    #[test]
    fn max_manipulability_tracks_the_target_and_never_scores_below_from_seed() {
        // Across random targets: the optimized solve hits the pose exactly, moves
        // psi at most the step bound from the FromSeed anchor, and never returns a
        // posture with lower manipulability than FromSeed's.
        let mut rng = StdRng::seed_from_u64(13);
        let mut fk = v1_fk("left");
        let lim = fk.limits();
        let m = ArmModel::from_fk(&mut fk).unwrap();
        let step = 0.05;
        let mut solved = 0;
        for _ in 0..40 {
            let q = sample_q(&mut rng, &lim);
            let target = pose(&mut fk, &q);
            let seed = sample_q(&mut rng, &lim);
            let Some(anchor) = solve(&m, &lim, &target, ArmAnglePolicy::FromSeed, &seed) else {
                continue;
            };
            let sol = solve(
                &m,
                &lim,
                &target,
                ArmAnglePolicy::MaxManipulability { max_step_rad: step },
                &seed,
            )
            .expect("solvable exactly when FromSeed is");
            let got = pose(&mut fk, &sol.q);
            assert!((got.translation.vector - target.translation.vector).norm() < 1e-6);
            assert!(got.rotation.angle_to(&target.rotation) < 1e-6);
            assert!(
                wrap_pi(sol.arm_angle - anchor.arm_angle).abs() <= step + 1e-9,
                "psi stepped past the bound"
            );
            let w_anchor = manipulability(&jacobian_poe(&m, &anchor.q));
            let w_opt = manipulability(&jacobian_poe(&m, &sol.q));
            assert!(
                w_opt >= w_anchor - 1e-12,
                "manipulability regressed: {w_opt} < {w_anchor}"
            );
            solved += 1;
        }
        assert!(solved > 20, "too few solvable samples ({solved}) to trust");
    }

    #[test]
    fn max_manipulability_chain_converges_and_climbs() {
        // Chaining solves at a fixed target (seeding each with the previous
        // solution, as the planner and the servo loop do) must climb toward a
        // manipulability ridge and settle there, not oscillate.
        let mut fk = v1_fk("left");
        let lim = fk.limits();
        let m = ArmModel::from_fk(&mut fk).unwrap();
        let q0 = [0.2, -0.3, 0.3, 1.0, -0.4, 0.5, 0.3];
        let target = pose(&mut fk, &q0);
        let mut seed = q0;
        let mut w_prev = manipulability(&jacobian_poe(&m, &q0));
        let mut psi_prev = arm_angle_of(&m, &q0).unwrap();
        let mut last_delta = f64::INFINITY;
        for _ in 0..60 {
            let sol = solve(
                &m,
                &lim,
                &target,
                ArmAnglePolicy::MaxManipulability { max_step_rad: 0.05 },
                &seed,
            )
            .expect("fixed reachable target stays solvable");
            let w = manipulability(&jacobian_poe(&m, &sol.q));
            assert!(w >= w_prev - 1e-12, "climb must be monotone");
            last_delta = wrap_pi(sol.arm_angle - psi_prev).abs();
            psi_prev = sol.arm_angle;
            w_prev = w;
            seed = sol.q;
        }
        assert!(
            last_delta < 1e-3,
            "psi still moving {last_delta} rad after 60 chained solves"
        );
    }

    #[test]
    fn from_seed_keeps_the_elbow_where_a_distant_seed_had_it() {
        // `FromSeed` promises continuity: keep the elbow's azimuth about the
        // shoulder-wrist axis. That azimuth has to be measured on the *target's*
        // redundancy circle, because that is the frame the feasible-psi intervals
        // and `solve_at_psi` live in. Measuring it on the seed's own circle
        // instead compares angles taken against two different references, and the
        // references diverge as the seed's wrist center moves away from the
        // target's - up to pi apart at arm's length.
        //
        // So: whenever the seed's (target-frame) azimuth is itself feasible,
        // `FromSeed` must return exactly that angle, however far the seed sits
        // from the target. Measuring in the wrong frame fails this immediately.
        let mut fk = v1_fk("left");
        let lim = fk.limits();
        let m = ArmModel::from_fk(&mut fk).unwrap();
        let mut rng = StdRng::seed_from_u64(0x5EED);

        let mut checked = 0;
        let mut far = 0;
        for _ in 0..3000 {
            let q_t = sample_q(&mut rng, &lim);
            let q_s = sample_q(&mut rng, &lim);
            if q_t[3] < 0.2 || q_s[3] < 0.2 {
                continue;
            }
            let target = pose(&mut fk, &q_t);
            let p_w = target.translation.vector;
            // Only distant seeds: a near seed cannot distinguish the two frames.
            if (p_w - fk_poe_position(&m, &q_s)).norm() < 0.15 {
                continue;
            }
            far += 1;

            let d = (p_w - m.shoulder).norm();
            let cos4 = ((d * d - m.l_su * m.l_su - m.l_uw * m.l_uw) / (2.0 * m.l_su * m.l_uw))
                .clamp(-1.0, 1.0);
            let ctx = PsiSolve {
                model: &m,
                limits: &lim,
                r_d: target.rotation.to_rotation_matrix(),
                p_w,
                theta4: cos4.acos(),
                circle: circle_frame(&m, p_w),
            };
            // Derived here from the target's circle directly, NOT via
            // `preferred_psi`: a test that asks the code under test what the
            // answer should be cannot fail when that code is wrong.
            let e = elbow_position(&m, &q_s) - ctx.circle.center;
            let (ax, bx) = (e.dot(&ctx.circle.a_hat), e.dot(&ctx.circle.b_hat));
            if ax.hypot(bx) < SINGULAR_RADIUS {
                continue;
            }
            let preferred = f64::atan2(bx, ax);
            // Only assert where the preference is actually attainable; otherwise
            // `nearest_feasible_psi` is correct to return something else.
            if ctx.solve_at_psi(preferred, &q_s).is_none() {
                continue;
            }

            let sol = solve(&m, &lim, &target, ArmAnglePolicy::FromSeed, &q_s)
                .expect("a feasible preferred psi must yield a solution");
            assert!(
                wrap_pi(sol.arm_angle - preferred).abs() < 1e-9,
                "FromSeed drifted off a feasible preference by {:.4} rad \
                 (got {:.4}, wanted {:.4})",
                wrap_pi(sol.arm_angle - preferred).abs(),
                sol.arm_angle,
                preferred,
            );
            checked += 1;
        }
        println!("distant seeds: {far}, with a feasible preference: {checked}");
        assert!(checked > 200, "too few distant feasible cases: {checked}");
    }
}
