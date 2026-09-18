//! Point-to-point inverse kinematics: searched, verified, and willing to refuse.
//!
//! [`Kinematics`] carries the surface frozen across this library and
//! `chain_kinematics_py`: [`inverse_kinematics`](Kinematics::inverse_kinematics)
//! for a planned move,
//! [`continue_to`](Kinematics::continue_to) for the next sample of a path,
//! [`track`](Kinematics::track) for a streamed setpoint, and
//! [`forward_kinematics`](Kinematics::forward_kinematics) to prove any of them.
//! Targets and reported poses are world-frame [`Isometry3`] values; the typed
//! rotation is what keeps a wire quaternion's component order from ever being
//! confused past the boundary.
//!
//! An accepted solution is proven: forward kinematics on the same chain puts it
//! inside the tolerance. A refusal is the search having tried every candidate
//! without one, not a proof of unreachability. A general chain has no closed
//! form to prove with; a 7-DOF SRS arm wanting proofs has `srs_model`.
//!
//! The search is bounded by work, never by the clock: it tries the same
//! candidates in the same order however fast the machine runs them, so an
//! answer is a function of its arguments alone. A wall-clock cutoff would make
//! a loaded CPU refuse targets an idle one solves, which is a plan that cannot
//! be validated on one machine and trusted on another.
//!
//! The internals are deliberately not part of that frozen surface. The damped
//! descent, the pose-ranked seed table and the two orientation passes are this
//! implementation's answer to the same problems the Python library measured its
//! own answers to (a local step has no notion of the workspace, a single
//! orientation weight is wrong in both directions, a descent at an unreachable
//! target orbits). The constants carry over with the mechanisms; the
//! qualification suite in `tests/ik_quality.rs` is what holds them to the
//! same measured bar.

use nalgebra::{Isometry3, SMatrix, SVector, Vector3};

use crate::rate::pose_error;
use crate::{Chain, Jacobian, Limit};

/// A solution is accepted when forward kinematics puts the end effector this
/// close to the requested position: pick_ik's `position_threshold` and MoveIt's
/// `constructGoalConstraints` default, and the resolution floor of a 12-bit
/// servo on a 0.4 m arm.
pub const ACCEPT_POSITION_M: f64 = 1e-3;
/// With no caller-named orientation tolerance this is only the first pass's
/// early exit, never a refusal criterion; it keeps the first position-feasible
/// candidate from being taken whatever its orientation.
pub const ACCEPT_ORIENTATION_RAD: f64 = std::f64::consts::PI / 180.0;
/// Drawn once at construction under a fixed generator, so an answer is a
/// function of its arguments and nothing else.
pub const SEED_TABLE_SIZE: usize = 2048;

/// Descent stops here, well inside the acceptance tolerance, so acceptance is
/// never decided by where an iteration happened to land. TRAC-IK's `eps`, KDL
/// LMA's `_eps` and MoveIt's KDL plugin `epsilon` all agree on 1e-5.
///
/// Doubles as the resolution of a kept answer: a candidate replaces the best
/// only by beating it by more than this. A finer difference is jitter, and
/// against an unreachable target it is the difference between coming to rest
/// and creeping along the workspace boundary a micron per call, forever.
const CONVERGED_M: f64 = 1e-5;
/// KDL's `ChainIkSolverPos_NR_JL` and pick_ik both cap at 100; stall detection
/// ends a spent descent long before this.
const MAX_ITERATIONS: usize = 100;
/// The step is a linearisation about the current configuration, so the position
/// error handed to it is capped at this stride and the target walked in.
/// Measured on a 5-DOF arm: 0.10 m costs no false refusals, 0.20 m costs two
/// per 1500; this sits a factor of four below the measured cliff.
const TRUST_REGION_M: f64 = 0.05;
/// A step that removes less than this fraction of the best error is not
/// progress. Measured against the best rather than the previous iteration,
/// because a descent orbiting an unreachable target dips on alternate steps.
const STALL_RATIO: f64 = 0.999;
const STALL_PATIENCE: u32 = 3;
/// Orientation's weight against the position objective's 1.0: the ranking's
/// metres-per-radian exchange rate, squared because the cost is quadratic, so
/// one trade governs both choosing a seed and descending from it. Swept on the
/// qualification suite: 1e-4 (the QP-shaped value) abandons 78% of attainable
/// orientations here, 1e-2 starts trading position away on the hard cases;
/// this value abandons 0.1% with zero refusals.
const ORIENTATION_WEIGHT: f64 = SEED_RANK_ANGLE_WEIGHT * SEED_RANK_ANGLE_WEIGHT;
/// Table candidates tried per pass beyond the caller's own seed. Python
/// measured 32 abandoning none of the orientations the chain can hold, where 8
/// abandoned 0.33% and 2 abandoned 4.7%.
///
/// This is also the search's cost ceiling: an unreachable target descends from
/// `1 + PASS_SEEDS` candidates twice and refuses. Measured worst case over 1000
/// unrelated-seed draws is 1.5 ms for a 7-DOF chain, 0.6 ms for a 5-DOF one.
const PASS_SEEDS: usize = 32;
/// Metres per radian when ranking a table entry's pose against the target.
/// Established equivalents span 0.01 (KDL LMA) to 1.0 (MoveIt's IK cache).
const SEED_RANK_ANGLE_WEIGHT: f64 = 0.05;
/// Damping for the descent's step. The same value as the streaming tick's
/// [`crate::DEFAULT_DLS_LAMBDA`] today, named apart because the two are tuned
/// against different costs: the tick against lag, this against refusals.
const SOLVE_DAMPING: f64 = 0.05;
const SEED_TABLE_RNG: u64 = 0;

/// A tolerance that failed to parse: zero, negative or non-finite. Zero reads
/// as "no preference" and means "nothing satisfies me"; NaN falsifies every
/// comparison and would surface as an unreachable pose.
#[derive(Debug, thiserror::Error)]
pub enum SolveError {
    #[error("{what} must be finite and positive, got {value}")]
    Tolerance { what: &'static str, value: f64 },
}

fn positive(value: f64, what: &'static str) -> Result<f64, SolveError> {
    (value.is_finite() && value > 0.0)
        .then_some(value)
        .ok_or(SolveError::Tolerance { what, value })
}

fn assert_finite_pose(pose: &Isometry3<f64>, what: &str) {
    assert!(
        pose.translation.vector.iter().all(|v| v.is_finite())
            && pose.rotation.coords.iter().all(|v| v.is_finite()),
        "{what} must be finite"
    );
}

/// A world-frame target pose, with the tolerances that decide its acceptance.
///
/// Parsed at construction: [`Goal::at`] rejects a non-finite pose, and the
/// `within` builders reject an unusable tolerance, so a [`Kinematics`] method
/// never sees a bound it cannot honestly test against. Tolerances left unnamed
/// fall back to the instance's; naming `orientation_within` also turns
/// orientation into an acceptance test rather than a best effort.
#[derive(Debug, Clone, Copy)]
pub struct Goal {
    target: Isometry3<f64>,
    position_m: Option<f64>,
    orientation_rad: Option<f64>,
}

impl Goal {
    pub fn at(target: Isometry3<f64>) -> Self {
        assert_finite_pose(&target, "goal target");
        Self {
            target,
            position_m: None,
            orientation_rad: None,
        }
    }

    pub fn position_within(mut self, meters: f64) -> Result<Self, SolveError> {
        self.position_m = Some(positive(meters, "position tolerance")?);
        Ok(self)
    }

    pub fn orientation_within(mut self, radians: f64) -> Result<Self, SolveError> {
        self.orientation_rad = Some(positive(radians, "orientation tolerance")?);
        Ok(self)
    }
}

struct TableEntry<const N: usize> {
    q: [f64; N],
    /// The entry's own end-effector pose, base frame, computed once at draw.
    pose: Isometry3<f64>,
}

/// FK and FK-verified IK over one chain: the point-to-point layer above
/// [`Chain`], holding the seed table and the instance defaults.
///
/// Plain data over a pure chain, so it is `Send + Sync` and every method takes
/// `&self`: answers are functions of their arguments.
pub struct Kinematics<const N: usize> {
    chain: Chain<N>,
    table: Vec<TableEntry<N>>,
    position_tolerance_m: f64,
}

impl<const N: usize> Kinematics<N> {
    pub fn new(chain: Chain<N>) -> Self {
        let table = draw_table(&chain, SEED_TABLE_SIZE);
        Self {
            chain,
            table,
            position_tolerance_m: ACCEPT_POSITION_M,
        }
    }

    /// The instance-wide acceptance tolerance; per-goal bounds override it.
    pub fn with_position_tolerance(mut self, meters: f64) -> Result<Self, SolveError> {
        self.position_tolerance_m = positive(meters, "position tolerance")?;
        Ok(self)
    }

    /// Redraw the table at another size, for a caller trading construction time
    /// against search breadth.
    pub fn with_seed_table(mut self, entries: usize) -> Self {
        self.table = draw_table(&self.chain, entries);
        self
    }

    pub fn chain(&self) -> &Chain<N> {
        &self.chain
    }

    pub fn limits(&self) -> [Limit; N] {
        self.chain.limits()
    }

    /// The end-effector pose at these joint positions, world frame.
    pub fn forward_kinematics(&self, joints: &[f64; N]) -> Isometry3<f64> {
        assert!(
            joints.iter().all(|v| v.is_finite()),
            "joint positions must be finite"
        );
        self.chain.world_pose(&self.chain.at(joints).ee_pose())
    }

    /// Joints reaching the goal inside its tolerances, or `None` when no
    /// candidate in either pass reached it. `None` is the search giving up,
    /// not a proof the pose is unreachable.
    ///
    /// The whole workspace is in scope: the caller's seed is tried first, then
    /// the table entries whose own poses rank nearest the target. With no named
    /// orientation tolerance, position alone is the acceptance test; a chain
    /// under six actuated joints cannot meet an arbitrary orientation, so the
    /// first pass takes the best orientation available and the second gives it
    /// up rather than lose the position with it.
    pub fn inverse_kinematics(&self, seed: &[f64; N], goal: &Goal) -> Option<[f64; N]> {
        let seed = self.clamped(seed);
        let target = self.chain.base_pose(&goal.target);
        let mut candidates = Vec::with_capacity(1 + PASS_SEEDS);
        candidates.push(seed);
        candidates.extend(self.ranked(&target));
        self.search(&candidates, &target, goal)
    }

    /// [`inverse_kinematics`](Self::inverse_kinematics) scoped to the seed's own neighbourhood: no table,
    /// no restarts, only a descent from the caller's seed. The primitive path
    /// following needs, where the answer must be the continuation of the
    /// previous sample and a distant reconfiguration is worse than a refusal.
    /// `None` means this neighbourhood did not reach it, and says nothing about
    /// the rest of the workspace.
    pub fn continue_to(&self, seed: &[f64; N], goal: &Goal) -> Option<[f64; N]> {
        let seed = self.clamped(seed);
        let target = self.chain.base_pose(&goal.target);
        self.search(&[seed], &target, goal)
    }

    /// The streamed path's step: always an in-limit configuration, never a
    /// refusal. A control tick's only vocabulary for failure is silence, and
    /// silence stops the arm mid-teleoperation.
    ///
    /// Against an unreachable target it comes to rest: the descent keeps the
    /// best configuration it reaches and the seed is among the candidates, so
    /// an unchanging target gives an unchanging answer once settled instead of
    /// walking the arm around the workspace boundary.
    pub fn track(&self, seed: &[f64; N], target: &Isometry3<f64>) -> [f64; N] {
        assert_finite_pose(target, "track target");
        let seed = self.clamped(seed);
        let target = self.chain.base_pose(target);
        self.descend(&seed, &target, ORIENTATION_WEIGHT)
    }

    /// Both passes over one candidate list: first with the orientation
    /// objective, then without it, acceptance identical in each.
    fn search(
        &self,
        candidates: &[[f64; N]],
        target: &Isometry3<f64>,
        goal: &Goal,
    ) -> Option<[f64; N]> {
        let tolerance = goal.position_m.unwrap_or(self.position_tolerance_m);
        let angle_bar = goal.orientation_rad.unwrap_or(ACCEPT_ORIENTATION_RAD);
        let orientation_gates = goal.orientation_rad.is_some();

        let mut best: Option<([f64; N], f64)> = None;
        for candidate in candidates {
            let joints = self.descend(candidate, target, ORIENTATION_WEIGHT);
            if let Some(reached) = self.verified(&joints, target, tolerance) {
                let angle = reached.rotation.angle_to(&target.rotation);
                if best.as_ref().is_none_or(|(_, held)| angle < *held) {
                    best = Some((joints, angle));
                }
                if angle <= angle_bar {
                    return Some(joints);
                }
            }
        }
        if let Some((joints, angle)) = best
            && (!orientation_gates || angle <= angle_bar)
        {
            return Some(joints);
        }
        // The second pass weighs orientation at zero, so it answers with an
        // arbitrary one; what it finds is still checked against a caller's bar.
        for candidate in candidates {
            let joints = self.descend(candidate, target, 0.0);
            if let Some(reached) = self.verified(&joints, target, tolerance)
                && (!orientation_gates || reached.rotation.angle_to(&target.rotation) <= angle_bar)
            {
                return Some(joints);
            }
        }
        None
    }

    /// Best joints the damped descent reaches from `seed`, always a
    /// configuration. The best is kept rather than the last, and the seed is
    /// among the candidates: at an unreachable target the descent orbits, so
    /// the iteration a loop happened to stop on must not decide the answer.
    fn descend(
        &self,
        seed: &[f64; N],
        target: &Isometry3<f64>,
        orientation_weight: f64,
    ) -> [f64; N] {
        let limits = self.chain.limits();
        let goal = target.translation.vector;
        let mut q = *seed;
        let mut best = q;
        let mut best_error = (self.chain.at(&q).ee_pose().translation.vector - goal).norm();
        let mut stalls = 0u32;
        for _ in 0..MAX_ITERATIONS {
            let posed = self.chain.at(&q);
            let here = posed.ee_pose();
            let (gap, dw) = pose_error(&here, target);
            let distance = gap.norm();
            let dp = if distance <= TRUST_REGION_M {
                gap
            } else {
                gap * (TRUST_REGION_M / distance)
            };
            let dq = damped_weighted_step(&posed.jacobian(), dp, dw, orientation_weight);
            for (value, (step, limit)) in q.iter_mut().zip(dq.iter().zip(limits.iter())) {
                *value = limit.clamp(*value + step);
            }
            let stepped = (self.chain.at(&q).ee_pose().translation.vector - goal).norm();
            stalls = if stepped <= STALL_RATIO * best_error {
                0
            } else {
                stalls + 1
            };
            if best_error - stepped > CONVERGED_M {
                best = q;
                best_error = stepped;
            }
            if best_error <= CONVERGED_M || stalls >= STALL_PATIENCE {
                break;
            }
        }
        best
    }

    /// The reached pose when `joints` is in limits and lands inside the
    /// position tolerance, else `None`. The proof behind every acceptance.
    fn verified(
        &self,
        joints: &[f64; N],
        target: &Isometry3<f64>,
        tolerance_m: f64,
    ) -> Option<Isometry3<f64>> {
        let limits = self.chain.limits();
        if !joints
            .iter()
            .zip(limits.iter())
            .all(|(v, l)| l.contains(*v))
        {
            return None;
        }
        let reached = self.chain.at(joints).ee_pose();
        ((reached.translation.vector - target.translation.vector).norm() <= tolerance_m)
            .then_some(reached)
    }

    /// The table entries whose own poses rank nearest the target, best first.
    /// Ranked rather than drawn at random: Python measured ranked seeds at an
    /// equal budget refusing none of 400 reachable poses where uniform restarts
    /// refused nine.
    fn ranked(&self, target: &Isometry3<f64>) -> Vec<[f64; N]> {
        let count = PASS_SEEDS.min(self.table.len());
        if count == 0 {
            return Vec::new();
        }
        let mut costs: Vec<(f64, usize)> = self
            .table
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let gap = (entry.pose.translation.vector - target.translation.vector).norm();
                let angle = entry.pose.rotation.angle_to(&target.rotation);
                (gap + SEED_RANK_ANGLE_WEIGHT * angle, index)
            })
            .collect();
        costs.select_nth_unstable_by(count - 1, |a, b| a.0.total_cmp(&b.0));
        costs.truncate(count);
        costs.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        costs
            .into_iter()
            .map(|(_, index)| self.table[index].q)
            .collect()
    }

    /// The seed brought into limits. Out-of-limit values are clamped, not
    /// refused; a non-finite value has no nearest in-limit neighbour and is a
    /// contract violation.
    fn clamped(&self, seed: &[f64; N]) -> [f64; N] {
        assert!(
            seed.iter().all(|v| v.is_finite()),
            "seed joints must be finite"
        );
        let limits = self.chain.limits();
        std::array::from_fn(|i| limits[i].clamp(seed[i]))
    }
}

/// One damped weighted least-squares step: minimise the position error, the
/// orientation error at `orientation_weight`, and the step's own size at
/// [`SOLVE_DAMPING`] squared. Solved through the N x N normal equations, whose
/// damping term makes them positive definite everywhere, singularities included.
fn damped_weighted_step<const N: usize>(
    jacobian: &Jacobian<N>,
    dp: Vector3<f64>,
    dw: Vector3<f64>,
    orientation_weight: f64,
) -> SVector<f64, N> {
    let position_rows = jacobian.fixed_rows::<3>(0);
    let rotation_rows = jacobian.fixed_rows::<3>(3);
    let normal = position_rows.transpose() * position_rows
        + (rotation_rows.transpose() * rotation_rows) * orientation_weight
        + SMatrix::<f64, N, N>::identity() * (SOLVE_DAMPING * SOLVE_DAMPING);
    let objective =
        position_rows.transpose() * dp + (rotation_rows.transpose() * dw) * orientation_weight;
    normal
        .cholesky()
        .expect("the damped normal matrix is positive definite for finite inputs")
        .solve(&objective)
}

/// The seed table: configurations drawn uniformly from the joint box under a
/// fixed SplitMix64 stream, each paired with its own end-effector pose.
fn draw_table<const N: usize>(chain: &Chain<N>, entries: usize) -> Vec<TableEntry<N>> {
    let limits = chain.limits();
    let mut rng = SplitMix64(SEED_TABLE_RNG);
    (0..entries)
        .map(|_| {
            let mut q = [0.0; N];
            for (value, limit) in q.iter_mut().zip(limits.iter()) {
                *value = limit.lo + rng.unit() * (limit.hi - limit.lo);
            }
            let pose = chain.at(&q).ee_pose();
            TableEntry { q, pose }
        })
        .collect()
}

/// SplitMix64: six lines of well-known constants rather than a dependency, and
/// deterministic by construction, which the table's contract needs.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1): the top 53 bits, a full mantissa's worth.
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }
}
