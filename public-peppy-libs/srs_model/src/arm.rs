//! The library entry point: one SRS arm loaded from a URDF, exposing forward
//! kinematics, gravity/Coriolis dynamics, and inverse kinematics behind a single
//! handle. Build it once and everything hangs off it; the underlying FK chain and
//! SRS model are internal.

use nalgebra::{Isometry3, Vector3};

use crate::fk::ForwardKinematics;
use crate::ik::{self, ArmAnglePolicy, Solution};
use crate::model::ArmModel;
use crate::{ARM_DOF, JointVec, Limit, Posed, SrsError};

/// Damping `lambda` for [`Arm::rate_step`] when the caller has no reason to pick
/// another. Re-exported from `chain_kinematics`, which owns the step: an arm and
/// the generic chain under it cannot be damped differently.
pub use chain_kinematics::DEFAULT_DLS_LAMBDA;

/// A complete SRS arm built from a URDF: forward kinematics + gravity/Coriolis
/// dynamics + closed-form inverse kinematics. The URDF is parsed once at
/// construction; pose it with [`at`](Self::at) for FK and dynamics, and solve
/// targets with [`solve_ik`](Self::solve_ik).
pub struct Arm {
    fk: ForwardKinematics,
    model: ArmModel,
}

impl Arm {
    /// Build from a URDF string, given the link where the 7-DOF SRS chain starts
    /// (`base_link`). Returns `Err` if the chain is missing, too short, or not a
    /// clean SRS arm.
    pub fn from_urdf(urdf: &str, base_link: &str) -> Result<Self, SrsError> {
        let mut fk = ForwardKinematics::from_urdf(urdf, base_link)?;
        let model = ArmModel::from_fk(&mut fk)?;
        Ok(Self { fk, model })
    }

    /// Like [`from_urdf`](Self::from_urdf) but reads the URDF from a file path,
    /// folding the IO error into the same `Result`.
    pub fn from_urdf_file(path: &str, base_link: &str) -> Result<Self, SrsError> {
        let mut fk = ForwardKinematics::from_urdf_file(path, base_link)?;
        let model = ArmModel::from_fk(&mut fk)?;
        Ok(Self { fk, model })
    }

    /// Raise the reported lower limit of joint `joint_idx` (0-based, j1..j7) to at
    /// least `floor`, returning the arm. The parsed URDF is left untouched: this is a
    /// control margin layered over the mechanical limit (e.g. holding a joint off a
    /// solver singularity), surfaced through [`limits`](Self::limits) so every
    /// consumer of the limits inherits it. Panics if `joint_idx >= ARM_DOF`.
    pub fn with_lower_floor(mut self, joint_idx: usize, floor: f64) -> Self {
        self.fk.set_lower_floor(joint_idx, floor);
        self
    }

    /// Mount the tool frame the URDF carries as `link_name`, which must sit below
    /// the chain tip on fixed joints only. The arm then speaks that frame
    /// throughout - [`at`](Self::at)'s [`ee_pose`](Posed::ee_pose), the
    /// [`jacobian`](Posed::jacobian) it is taken at, [`solve_ik`](Self::solve_ik)'s
    /// target, and the twist [`rate_step`](Self::rate_step) realizes - so a caller
    /// cannot command one frame and read another. Without this an arm controls its
    /// bare tip.
    ///
    /// Taking the frame from the URDF rather than from a caller's numbers keeps the
    /// tool where the rest of the robot's geometry lives, and makes it a rigid
    /// transform by construction. Errors if the link is absent or is reached
    /// through a joint that moves.
    pub fn with_tool_link(mut self, link_name: &str) -> Result<Self, SrsError> {
        self.fk.set_tool_link(link_name)?;
        Ok(self)
    }

    /// The arm as a plain serial chain: what the topology-agnostic operations in
    /// [`chain_kinematics`] take. The chain carries this arm's tool frame and its
    /// limits, elbow floor included, so a generic law run over it is run over the
    /// same arm the SRS-specific methods here describe.
    pub fn chain(&self) -> &chain_kinematics::Chain<ARM_DOF> {
        self.fk.chain()
    }

    /// Pose the arm at configuration `q` for forward-kinematics and dynamics
    /// reads. The returned [`Posed`] is a snapshot of that configuration's frames,
    /// so it can be held across other reads and two configurations can be compared
    /// side by side. See [`Posed::ee_pose`], [`Posed::gravity_torques`],
    /// [`Posed::coriolis_torques`].
    pub fn at(&self, q: &JointVec) -> Posed<'_> {
        self.fk.at(q)
    }

    /// Solve inverse kinematics for a `target` EE pose in the **arm base frame**,
    /// resolving the redundant arm angle per `arm_angle` and selecting the branch
    /// nearest `seed`. Convert a world-frame target with [`base_pose`](Self::base_pose)
    /// first. `None` if the target is unreachable or admits no in-limit solution.
    /// In-limit means [`limits`](Self::limits), so a floor set by
    /// [`with_lower_floor`](Self::with_lower_floor) constrains the solutions too.
    ///
    /// `target` is where the caller wants the end-effector, so on an arm with a
    /// tool it is a tool-frame target; the tool is removed here to leave the tip
    /// target the closed form is defined on.
    pub fn solve_ik(
        &self,
        target: &Isometry3<f64>,
        arm_angle: ArmAnglePolicy,
        seed: &JointVec,
    ) -> Option<Solution> {
        ik::solve(
            &self.model,
            &self.fk.limits(),
            &(target * self.fk.tool().inverse()),
            arm_angle,
            seed,
        )
    }

    /// The arm angle of configuration `q`, or `None` at the straight-arm
    /// singularity where it is geometrically undefined.
    pub fn arm_angle(&self, q: &JointVec) -> Option<f64> {
        ik::arm_angle_of(&self.model, q)
    }

    /// URDF joint position limits, j1..j7, in radians.
    pub fn limits(&self) -> [Limit; ARM_DOF] {
        self.fk.limits()
    }

    /// The mounted `tip -> tool` transform, identity when none is mounted. The tool
    /// origin's distance from the tip bounds how far the end-effector can be from
    /// the tip in any direction, which a caller reasoning about reach needs.
    pub fn tool(&self) -> Isometry3<f64> {
        self.fk.tool()
    }

    /// Convert a world/body-frame pose into the arm base frame the solver uses.
    pub fn base_pose(&self, world: &Isometry3<f64>) -> Isometry3<f64> {
        self.model.base_pose(world)
    }

    /// Convert an arm-base-frame pose (e.g. FK output) back into the world frame.
    pub fn world_pose(&self, base: &Isometry3<f64>) -> Isometry3<f64> {
        self.model.world_pose(base)
    }

    /// The fixed `world -> base` mount transform resolved from the URDF. It is
    /// identity when `base_link` is the URDF root (no mount tree above it); since
    /// gravity/Coriolis are evaluated in that frame, a caller can log/verify which
    /// frame is in play rather than assume one.
    pub fn base_from_world(&self) -> Isometry3<f64> {
        self.fk.base_from_world()
    }

    /// One damped resolved-rate joint step at `q` toward a world-frame task
    /// increment: `dp_world` metres of end-effector translation and `dw_world`
    /// axis-angle radians of rotation, either of which may be zero to softly
    /// hold that component. The caller caps the increments to its speed budgets;
    /// this rotates them into the arm base frame, solves
    /// `dq = J⁺(λ) ξ` with the damped pseudo-inverse (bounded through
    /// singularities), scales `dq` so every joint respects its velocity budget
    /// over `dt_s` while preserving direction, and clamps the result into the
    /// position limits. The step the operator streaming jog and the backbone's
    /// guarded servo both run, shared so the two control paths cannot drift.
    pub fn rate_step(
        &self,
        q: &JointVec,
        dp_world: Vector3<f64>,
        dw_world: Vector3<f64>,
        max_joint_velocity_rad_s: &JointVec,
        dt_s: f64,
        lambda: f64,
    ) -> JointVec {
        chain_kinematics::rate_step(
            self.fk.chain(),
            q,
            dp_world,
            dw_world,
            max_joint_velocity_rad_s,
            dt_s,
            lambda,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/openarm_v10.urdf"
    );

    #[test]
    fn from_urdf_file_loads_and_reports_limits() {
        let arm = Arm::from_urdf_file(FIXTURE, "openarm_left_link0").expect("load fixture");
        let limits = arm.limits();
        for (i, l) in limits.iter().enumerate() {
            assert!(l.lo <= l.hi, "joint {i}: lo {} > hi {}", l.lo, l.hi);
        }
        // j4 (elbow) is one-sided in the V1.0 URDF: lower bound at 0.
        assert!(limits[3].lo.abs() < 1e-9, "j4 lower = {}", limits[3].lo);
    }

    #[test]
    fn with_lower_floor_raises_only_the_targeted_lower_bound() {
        let arm = Arm::from_urdf_file(FIXTURE, "openarm_left_link0").expect("load fixture");
        let base = arm.limits();
        // The fixture's elbow (j4, index 3) has a mechanical lower bound of 0.0.
        let floored = arm.with_lower_floor(3, 0.05).limits();

        assert_eq!(
            floored[3].lo, 0.05,
            "targeted joint's lower bound is raised"
        );
        assert_eq!(floored[3].hi, base[3].hi, "upper bound is untouched");
        for i in [0, 1, 2, 4, 5, 6] {
            assert_eq!(
                floored[i].lo, base[i].lo,
                "joint {i} lower bound is untouched"
            );
        }
    }

    #[test]
    fn a_lower_floor_constrains_the_solutions_not_just_the_report() {
        // The floor exists to hold a joint off a singularity, which it cannot do if
        // IK keeps returning configurations under it. Uses a floor far from the
        // straight-arm singularity so a refusal here is the limit doing the work
        // rather than the solver failing on conditioning.
        const FLOOR: f64 = 0.5;
        let arm = Arm::from_urdf_file(FIXTURE, "openarm_left_link0")
            .expect("load fixture")
            .with_lower_floor(3, FLOOR);
        assert_eq!(arm.limits()[3].lo, FLOOR, "the floor is reported");

        let under: JointVec = [0.1, 0.2, 0.0, 0.30, 0.0, 0.3, 0.0];
        let target = arm.at(&under).ee_pose();
        if let Some(s) = arm.solve_ik(&target, ArmAnglePolicy::FromSeed, &under) {
            assert!(
                s.q[3] >= FLOOR,
                "IK returned j4 = {} under the {FLOOR} floor it reports",
                s.q[3]
            );
        }
    }

    #[test]
    fn with_lower_floor_below_the_mechanical_limit_is_a_noop() {
        let arm = Arm::from_urdf_file(FIXTURE, "openarm_left_link0").expect("load fixture");
        let base = arm.limits();
        let floored = arm.with_lower_floor(3, -10.0).limits();
        assert_eq!(
            floored[3].lo, base[3].lo,
            "a floor under the limit does not lower it"
        );
    }

    #[test]
    fn from_urdf_file_missing_path_errors_with_path() {
        // `Arm` is not `Debug` (the FK chain isn't), so match rather than unwrap_err.
        let err = match Arm::from_urdf_file("/no/such/file.urdf", "openarm_left_link0") {
            Ok(_) => panic!("expected an error for a missing path"),
            Err(e) => e,
        };
        assert!(
            matches!(&err, SrsError::UrdfRead { path, .. } if path == "/no/such/file.urdf"),
            "error should name the path: {err}"
        );
    }

    const RATE_Q: JointVec = [0.3, 0.1, 0.2, 0.8, 0.3, 0.2, 0.15];
    const RATE_V_MAX: JointVec = [3.0; ARM_DOF];
    const RATE_DT: f64 = 0.01;
    const RATE_LAMBDA: f64 = 0.05;

    fn rate_arm() -> Arm {
        Arm::from_urdf_file(FIXTURE, "openarm_left_link0").expect("load fixture")
    }

    fn ee_world(arm: &mut Arm, q: &JointVec) -> Isometry3<f64> {
        let base = arm.at(q).ee_pose();
        arm.world_pose(&base)
    }

    fn tip_world(arm: &mut Arm, q: &JointVec) -> Isometry3<f64> {
        let base = arm.at(q).tip_pose();
        arm.world_pose(&base)
    }

    #[test]
    fn rate_step_moves_the_ee_along_the_commanded_direction() {
        let mut arm = rate_arm();
        let before = ee_world(&mut arm, &RATE_Q);
        let dp = Vector3::new(3e-3, 0.0, 0.0);
        let q = arm.rate_step(
            &RATE_Q,
            dp,
            Vector3::zeros(),
            &RATE_V_MAX,
            RATE_DT,
            RATE_LAMBDA,
        );
        let after = ee_world(&mut arm, &q);
        let moved = after.translation.vector - before.translation.vector;
        assert!(
            moved.dot(&dp) / dp.norm_squared() > 0.5,
            "step must realize most of the commanded translation, got {moved:?}"
        );
        assert!(
            after.rotation.angle_to(&before.rotation) < 5e-3,
            "an untasked orientation is softly held"
        );
    }

    #[test]
    fn rate_step_respects_the_velocity_budget_and_limits() {
        let arm = rate_arm();
        // An absurd demand: the scaling must keep every joint inside its budget.
        let dp = Vector3::new(1.0, -1.0, 0.5);
        let q = arm.rate_step(
            &RATE_Q,
            dp,
            Vector3::zeros(),
            &RATE_V_MAX,
            RATE_DT,
            RATE_LAMBDA,
        );
        let limits = arm.limits();
        for i in 0..ARM_DOF {
            let v = (q[i] - RATE_Q[i]).abs() / RATE_DT;
            assert!(v <= RATE_V_MAX[i] * 1.0001, "joint {i} at {v:.2} rad/s");
            assert!(q[i] >= limits[i].lo && q[i] <= limits[i].hi);
        }
    }

    #[test]
    fn rate_step_holds_still_on_a_zero_task() {
        let arm = rate_arm();
        let q = arm.rate_step(
            &RATE_Q,
            Vector3::zeros(),
            Vector3::zeros(),
            &RATE_V_MAX,
            RATE_DT,
            RATE_LAMBDA,
        );
        assert_eq!(q, RATE_Q, "zero task must not move any joint");
    }

    // The fixture carries `openarm_{side}_tcp`, a frame fixed well off the tip, so
    // a test that passes cannot be one that ignores the tool.
    const TOOL_LINK: &str = "openarm_left_tcp";

    fn tooled_arm() -> Arm {
        rate_arm()
            .with_tool_link(TOOL_LINK)
            .expect("the fixture carries the tool link")
    }

    #[test]
    fn a_mounted_tool_moves_the_ee_frame_and_leaves_the_tip_where_it_was() {
        let bare = rate_arm();
        let bare_tip = bare.at(&RATE_Q).tip_pose();
        let bare_ee = bare.at(&RATE_Q).ee_pose();
        assert_eq!(bare_tip, bare_ee, "with no tool the EE frame is the tip");

        let tooled = tooled_arm();
        assert_eq!(
            tooled.at(&RATE_Q).tip_pose(),
            bare_tip,
            "mounting a tool must not move the tip"
        );
        let tool = tooled.tool();
        assert!(
            tool.translation.vector.norm() > 0.1,
            "the fixture's tool link should sit well off the tip, got {tool:?}"
        );
        let ee = tooled.at(&RATE_Q).ee_pose();
        let expected = bare_tip * tool;
        assert!(
            (ee.translation.vector - expected.translation.vector).norm() < 1e-12
                && ee.rotation.angle_to(&expected.rotation) < 1e-12,
            "EE frame must be the tip composed with the tool: {ee:?} vs {expected:?}"
        );
    }

    #[test]
    fn ik_round_trips_the_tool_frame_it_was_given() {
        // The whole point of the seam: a target expressed at the tool is reached at
        // the tool. A composition inverted anywhere lands a tool-length away.
        let arm = tooled_arm();
        let target = arm.at(&RATE_Q).ee_pose();
        let seed: JointVec = std::array::from_fn(|i| RATE_Q[i] + 0.05);
        let solution = arm
            .solve_ik(&target, ArmAnglePolicy::FromSeed, &seed)
            .expect("a pose reached by FK is reachable");
        let reached = arm.at(&solution.q).ee_pose();
        assert!(
            (reached.translation.vector - target.translation.vector).norm() < 1e-9,
            "IK reached {:?}, wanted {:?}",
            reached.translation.vector,
            target.translation.vector
        );
        assert!(
            reached.rotation.angle_to(&target.rotation) < 1e-9,
            "IK reached a {:.6} rad different orientation",
            reached.rotation.angle_to(&target.rotation)
        );
    }

    #[test]
    fn a_pure_rotation_pivots_about_the_tool_point() {
        // The sharpest statement that the Jacobian moved with the frame: a rotation
        // task holds the point it is taken at and swings everything else. Left at
        // the tip, this would drag the tool point through a tool-length arc.
        let mut arm = tooled_arm();
        let tip_before = tip_world(&mut arm, &RATE_Q);
        let ee_before = ee_world(&mut arm, &RATE_Q);
        let dw = Vector3::new(0.0, 0.0, 1e-2);
        let q = arm.rate_step(
            &RATE_Q,
            Vector3::zeros(),
            dw,
            &RATE_V_MAX,
            RATE_DT,
            RATE_LAMBDA,
        );
        let tip_after = tip_world(&mut arm, &q);
        let ee_after = ee_world(&mut arm, &q);

        let turned = ee_after.rotation.angle_to(&ee_before.rotation);
        assert!(
            (turned - dw.norm()).abs() < 1e-4,
            "the commanded rotation must be realized, got {turned}"
        );
        let tip_swing = (tip_after.translation.vector - tip_before.translation.vector).norm();
        let tool_swing = (ee_after.translation.vector - ee_before.translation.vector).norm();
        assert!(
            tip_swing > 1e-4,
            "the tip should swing about the tool point, got {tip_swing}"
        );
        // The damped step trades a little drift for conditioning, so hold is an
        // order of magnitude, not zero: the pivot must move far less than what it
        // is swinging.
        assert!(
            tool_swing < 0.2 * tip_swing,
            "the tool point should hold: moved {tool_swing} against a {tip_swing} tip swing"
        );
    }

    #[test]
    fn with_tool_link_rejects_anything_whose_offset_could_move() {
        for (label, link) in [
            ("absent from the URDF", "openarm_left_nonesuch"),
            // Reached through a prismatic finger joint, so its offset tracks the
            // gripper opening rather than staying put.
            ("below a movable joint", "openarm_left_left_finger"),
            // A real link, but up the arm rather than below the tip.
            ("not below the tip", "openarm_left_link3"),
        ] {
            let err = match rate_arm().with_tool_link(link) {
                Ok(_) => panic!("{label}: expected a rejection for '{link}'"),
                Err(e) => e,
            };
            assert!(
                matches!(err, SrsError::Tool(_)),
                "{label}: wrong error {err}"
            );
        }
    }
}
