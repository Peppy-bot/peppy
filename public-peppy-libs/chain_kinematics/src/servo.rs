//! A guarded Cartesian move: the damped resolved-rate law run against a
//! reference that walks toward the goal, leashed whenever the chain falls
//! behind.
//!
//! A discrete inverse-kinematics walk cannot cross the singular surface between
//! two solution branches. The damped law can: the damping bounds the joint rates
//! while the task error carries the chain across, deviating from the straight
//! line only where the geometry forces it and re-converging beyond. Leashing the
//! reference is what keeps that honest - the goal never runs away from an arm
//! that is grinding through a wall.
//!
//! The same law rolls out offline ([`rollout`]) before a move is accepted, which
//! is the reachability check: a goal that does not converge within the caller's
//! budget is refused rather than started. That only works because the law is
//! deterministic, so the motion that was validated is the motion that runs.
//!
//! Tolerances and the output smoother are the caller's, not this crate's. A
//! smoother is whatever bounds the command's jerk; anchoring the numbers to a
//! particular controller's defaults would make this crate carry that
//! controller's dependencies for no reason.

use nalgebra::{Isometry3, Vector3};

use crate::rate::{DEFAULT_DLS_LAMBDA, pose_error, rate_step};
use crate::{Chain, Limit};

/// The end-effector speed budget a Cartesian step runs under.
#[derive(Debug, Clone, Copy)]
pub struct EeCaps {
    pub linear_m_s: f64,
    pub angular_rad_s: f64,
}

/// How close counts as arrived. Arrival is all it decides: the step deadband
/// and the degenerate-line guard are the law's own ([`TRACKING_FLOOR_M`]), so a
/// caller's slack changes when a move is done, never how it is tracked.
///
/// Parsed rather than taken as given, because the two are not independent: a
/// position tolerance below the law's own floor is one the law stops correcting
/// toward before it is met, so a move asking for it would run its whole budget
/// and time out. Refused here, once, instead of read back as an unreachable goal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ServoTolerances {
    position_m: f64,
    orientation_rad: f64,
}

impl ServoTolerances {
    /// The arrival slack a move is judged against, or a refusal when the law
    /// could not reach it.
    pub fn new(position_m: f64, orientation_rad: f64) -> Result<Self, ToleranceError> {
        if !(position_m.is_finite() && position_m >= TRACKING_FLOOR_M) {
            return Err(ToleranceError::Position(position_m));
        }
        if !(orientation_rad.is_finite() && orientation_rad > 0.0) {
            return Err(ToleranceError::Orientation(orientation_rad));
        }
        Ok(Self {
            position_m,
            orientation_rad,
        })
    }

    pub fn position_m(&self) -> f64 {
        self.position_m
    }

    pub fn orientation_rad(&self) -> f64 {
        self.orientation_rad
    }
}

/// An arrival slack the law cannot honour. Plain data, so a caller can carry the
/// refusal in a type of its own without giving up `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum ToleranceError {
    #[error(
        "position tolerance {0} m is not a finite distance at or above the law's          tracking floor of {TRACKING_FLOOR_M} m, below which it stops correcting"
    )]
    Position(f64),
    #[error("orientation tolerance {0} rad is not a finite positive angle")]
    Orientation(f64),
}

/// Position error below which a step stops correcting position, and below which
/// a line is a pure reorientation. The tracking floor of the law itself, and the
/// guard on the division by the line's length. A caller cannot ask to arrive
/// tighter than this: [`ServoTolerances::new`] refuses it, and at exactly this
/// error a step still corrects, so the floor itself is an arrival that lands.
pub const TRACKING_FLOOR_M: f64 = 1e-3;

/// Everything one servo tick runs under: the per-joint velocity budget, the
/// end-effector speed caps, the arrival tolerances, and the control period.
///
/// Grouped because they are one decision - "how this move is allowed to go" -
/// taken once by the caller and then threaded unchanged through every step, and
/// because a step is easier to get right when its budgets cannot be passed in
/// the wrong order.
#[derive(Debug, Clone, Copy)]
pub struct ServoLimits<const N: usize> {
    pub max_joint_velocity: [f64; N],
    pub ee: EeCaps,
    pub tolerances: ServoTolerances,
    pub dt_s: f64,
}

/// Per-joint output smoothing, bounding the jerk of the commanded step.
///
/// A trait rather than a concrete filter so this crate needs no filter library:
/// the caller passes whatever its controller already uses, and the servo's
/// output is then smoothed exactly as the rest of that controller's commands are.
pub trait Smoother<const N: usize> {
    fn smooth(&mut self, q: &[f64; N]) -> [f64; N];
}

/// A smoother that does nothing, for a caller that bounds jerk elsewhere or not
/// at all.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSmoothing;

impl<const N: usize> Smoother<N> for NoSmoothing {
    fn smooth(&mut self, q: &[f64; N]) -> [f64; N] {
        *q
    }
}

/// One damped resolved-rate step toward a pose target, given the end-effector
/// pose already computed at `q`.
///
/// The task error is capped before it is resolved: position at the speed budget,
/// orientation at the slew budget. A target metres away therefore produces the
/// same bounded step as one millimetres away, which is what lets one law serve
/// both a planned move and a live stream.
///
/// It always returns a configuration. An unreachable target moves toward the
/// closest pose this step can find and saturates there rather than refusing,
/// because a teleoperator pushing past the edge should feel a wall, not a
/// disconnect.
pub fn rate_step_toward<const N: usize>(
    chain: &Chain<N>,
    q: &[f64; N],
    ee: &Isometry3<f64>,
    target: &Isometry3<f64>,
    limits: &ServoLimits<N>,
) -> [f64; N] {
    let (caps, dt_s) = (limits.ee, limits.dt_s);
    let (dp, dw) = pose_error(ee, target);
    // A deadband on position only: rotation has none. Inclusive at the floor, so
    // the set this stops correcting in is exactly the set the tightest accepted
    // tolerance calls arrived; an error of exactly the floor would otherwise be
    // neither corrected nor converged.
    let dp = if dp.norm() >= TRACKING_FLOOR_M {
        dp * (caps.linear_m_s * dt_s / dp.norm()).min(1.0)
    } else {
        Vector3::zeros()
    };
    let dw = if dw.norm() > 0.0 {
        dw * (caps.angular_rad_s * dt_s / dw.norm()).min(1.0)
    } else {
        Vector3::zeros()
    };
    rate_step(
        chain,
        q,
        dp,
        dw,
        &limits.max_joint_velocity,
        dt_s,
        DEFAULT_DLS_LAMBDA,
    )
}

/// One servo move's state: where the reference has reached along the line, and
/// the output smoother. The joint state stays with the caller, which each tick
/// advances.
pub struct ServoState<const N: usize, S: Smoother<N>> {
    start: Isometry3<f64>,
    end: Isometry3<f64>,
    /// Reference progress along the line, 0..=1.
    reference_s: f64,
    /// The reference stops while the chain is farther than this from it, so a
    /// wall is ground through instead of the reference running away.
    leash_m: f64,
    smoother: S,
}

/// One tick's outcome.
#[derive(Debug, Clone, Copy)]
pub enum ServoStep<const N: usize> {
    /// Advanced: the new joint setpoint to command.
    Stepped([f64; N]),
    /// Reached the goal pose within tolerance.
    Converged([f64; N]),
}

impl<const N: usize, S: Smoother<N>> ServoState<N, S> {
    pub fn new(start: Isometry3<f64>, end: Isometry3<f64>, leash_m: f64, smoother: S) -> Self {
        Self {
            start,
            end,
            reference_s: 0.0,
            leash_m,
            smoother,
        }
    }

    /// Distance (m) from the end effector at `q` to the goal position. What a
    /// caller that gives up on a move reports: how far short it stopped.
    pub fn position_err_m(&self, chain: &Chain<N>, q: &[f64; N]) -> f64 {
        let ee = chain.world_pose(&chain.at(q).ee_pose());
        (self.end.translation.vector - ee.translation.vector).norm()
    }

    /// Advance one tick of `dt_s`: walk the reference (leashed to the chain),
    /// take one damped resolved-rate step toward it, and report whether the goal
    /// pose is reached.
    pub fn step(
        &mut self,
        chain: &Chain<N>,
        q: &[f64; N],
        limits: &ServoLimits<N>,
    ) -> ServoStep<N> {
        let (caps, tol, dt_s) = (limits.ee, limits.tolerances, limits.dt_s);
        let ee = chain.world_pose(&chain.at(q).ee_pose());

        let goal_pos_err = (self.end.translation.vector - ee.translation.vector).norm();
        let goal_rot_err = ee.rotation.angle_to(&self.end.rotation);
        if self.reference_s >= 1.0
            && goal_pos_err < tol.position_m()
            && goal_rot_err < tol.orientation_rad()
        {
            return ServoStep::Converged(*q);
        }

        // Walk the reference at the speed cap while the chain holds the leash; a
        // zero-length line (pure reorientation) starts fully advanced.
        let line_len = (self.end.translation.vector - self.start.translation.vector).norm();
        let reference = interpolate(&self.start, &self.end, self.reference_s);
        let ref_pos_err = (reference.translation.vector - ee.translation.vector).norm();
        if line_len < TRACKING_FLOOR_M {
            self.reference_s = 1.0;
        } else if ref_pos_err < self.leash_m {
            self.reference_s = (self.reference_s + caps.linear_m_s * dt_s / line_len).min(1.0);
        }
        let reference = interpolate(&self.start, &self.end, self.reference_s);

        let next = rate_step_toward(chain, q, &ee, &reference, limits);
        // Smooth, then re-clamp to the velocity limit: a smoother can overshoot
        // its input, so without this the smoothed command could exceed the limit
        // the step enforced. The clamp is the final stage, so the commanded
        // velocity holds regardless of the filter transient.
        let smoothed = self.smoother.smooth(&next);
        let joint_limits: [Limit; N] = chain.limits();
        ServoStep::Stepped(std::array::from_fn(|i| {
            let cap = limits.max_joint_velocity[i] * dt_s;
            joint_limits[i].clamp(q[i] + (smoothed[i] - q[i]).clamp(-cap, cap))
        }))
    }
}

/// Roll the servo law out offline at the control period per step: the plan-time
/// proof that it reaches the pose, and how long that takes, or `None` when it has
/// not converged within `budget_s`.
///
/// Deterministic and identical to the runtime law, so an accepted goal executes
/// the motion that was validated.
///
/// Takes the state by value: a rollout spends it, walking the reference to the
/// goal, and a spent one restarted mid-line would answer for a move nobody asked
/// for. The runtime builds its own.
pub fn rollout<const N: usize, S: Smoother<N>>(
    chain: &Chain<N>,
    mut state: ServoState<N, S>,
    seed: [f64; N],
    limits: &ServoLimits<N>,
    budget_s: f64,
) -> Option<f64> {
    let mut q = seed;
    let steps = (budget_s / limits.dt_s).ceil() as usize;
    for k in 0..steps {
        match state.step(chain, &q, limits) {
            ServoStep::Stepped(next) => q = next,
            ServoStep::Converged(_) => return Some(k as f64 * limits.dt_s),
        }
    }
    None
}

/// Below this separation two orientations are one orientation, and the arc
/// between them is whatever the last few bits of the quaternion happened to be.
/// Slerp divides by the sine of that arc, so it is answered with an endpoint
/// instead: 1e-6 rad is nine orders below any tolerance a chain is servoed to.
const COINCIDENT_ARC_EPS: f64 = 1e-6;

/// Pose on the straight line between two poses: position lerped, orientation
/// slerped along the shorter arc.
///
/// A move that holds its orientation reaches the coincident case on every tick,
/// where the interpolation is a ratio of two vanishing sines. Taking the endpoint
/// there keeps a held orientation exactly held rather than accumulating that
/// residual over the length of a move.
pub fn interpolate(a: &Isometry3<f64>, b: &Isometry3<f64>, s: f64) -> Isometry3<f64> {
    let s = s.clamp(0.0, 1.0);
    Isometry3::from_parts(
        (a.translation.vector + s * (b.translation.vector - a.translation.vector)).into(),
        a.rotation
            .try_slerp(&b.rotation, s, COINCIDENT_ARC_EPS)
            .unwrap_or(b.rotation),
    )
}
