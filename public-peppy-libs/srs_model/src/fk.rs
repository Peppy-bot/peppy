//! The 7-DOF SRS arm as a [`chain_kinematics::Chain`], plus the rule that makes
//! it one.
//!
//! [`chain_kinematics`] poses any serial path through any URDF; what is specific
//! to this arm is *which* path. The wrist is found by walking exactly [`ARM_DOF`]
//! revolute joints out from the base, so the 7-DOF SRS invariant is enforced
//! rather than trusting a hand-entered tip that might disagree, and everything
//! past the wrist (gripper, fingers, tools) becomes the distal payload the
//! dynamics carry.
//!
//! Poses come back in the **arm base frame**, lining up with the SRS geometry in
//! [`crate::model`] and the IK target frame. [`crate::gravity`] and
//! [`crate::coriolis`] read the world-frame accessors instead, because gravity is
//! a world quantity (it acts along world -Z).

use chain_kinematics::{Chain, ChainSpec, JointKind, JointSelection, Tree};
use nalgebra::Isometry3;

use crate::SrsError;
use crate::{ARM_DOF, JointVec, Limit, Posed};

/// The arm's chain, with the SRS wrist already resolved.
pub(crate) struct ForwardKinematics {
    chain: Chain<ARM_DOF>,
}

impl ForwardKinematics {
    /// Build from a URDF string given only where the SRS arm *starts*
    /// (`base_link`). Agnostic to *which* 7-DOF SRS arm: any URDF and base link
    /// the caller passes (it is not a general N-DOF or non-SRS solver - for that,
    /// use [`chain_kinematics`] directly).
    pub fn from_urdf(urdf: &str, base_link: &str) -> Result<Self, SrsError> {
        let robot = urdf_rs::read_from_string(urdf).map_err(|e| format!("parse URDF: {e}"))?;
        let tree = Tree::from_robot(&robot).map_err(SrsError::NotSrsArm)?;
        let base = tree
            .link_index(base_link)
            .ok_or_else(|| format!("URDF missing base link '{base_link}'"))?;
        let tip = find_srs_tip(&tree, base)?;
        let tip_link = tree.link(tip).name.clone();
        let chain = Chain::<ARM_DOF>::from_tree(
            tree,
            &ChainSpec {
                base_link: Some(base_link),
                tip_link: &tip_link,
                joints: JointSelection::PathOrder,
            },
        )
        .map_err(|e| SrsError::NotSrsArm(e.to_string()))?;
        Ok(Self { chain })
    }

    /// Like [`from_urdf`](Self::from_urdf) but reads the URDF from a file path,
    /// folding the IO error into the same `Result`.
    pub fn from_urdf_file(path: &str, base_link: &str) -> Result<Self, SrsError> {
        let urdf = std::fs::read_to_string(path).map_err(|source| SrsError::UrdfRead {
            path: path.to_string(),
            source,
        })?;
        Self::from_urdf(&urdf, base_link)
    }

    /// URDF joint position limits, j1..j7, in radians, including any control
    /// floor layered over them by [`set_lower_floor`](Self::set_lower_floor).
    pub fn limits(&self) -> [Limit; ARM_DOF] {
        self.chain.limits()
    }

    /// Raise the reported lower bound of joint `joint_idx` to at least `floor`,
    /// leaving the parsed URDF untouched. Used to impose a control margin the URDF
    /// itself should not carry (the mechanical limit stays as vendored). Panics if
    /// `joint_idx >= ARM_DOF`.
    pub(crate) fn set_lower_floor(&mut self, joint_idx: usize, floor: f64) {
        assert!(
            joint_idx < ARM_DOF,
            "joint_idx {joint_idx} out of range (ARM_DOF={ARM_DOF})"
        );
        self.chain.set_lower_floor(joint_idx, floor);
    }

    /// Mount the tool frame carried by `link_name`, which must sit below the tip
    /// on fixed joints only. The SRS geometry the IK solver is derived from stays
    /// on the tip ([`Posed::tip_pose`]), so mounting a tool never moves the wrist
    /// centre the closed form is built around.
    pub(crate) fn set_tool_link(&mut self, link_name: &str) -> Result<(), SrsError> {
        self.chain
            .set_tool_link(link_name)
            .map_err(|e| SrsError::Tool(e.to_string()))
    }

    /// The mounted `tip -> tool` transform (identity when none is mounted).
    pub(crate) fn tool(&self) -> Isometry3<f64> {
        self.chain.tool()
    }

    /// The underlying generic chain, for the arm-level operations that are just
    /// the generic law applied at seven joints.
    pub fn chain(&self) -> &Chain<ARM_DOF> {
        &self.chain
    }

    /// Pose the arm at configuration `q` for forward-kinematics and dynamics
    /// reads.
    pub fn at(&self, q: &JointVec) -> Posed<'_> {
        self.chain.at(q)
    }

    /// The fixed `world -> base` mount transform resolved from the URDF. It is
    /// **identity** when `base_link` is the URDF root (no mount tree above it),
    /// i.e. gravity is then computed in the base frame. Exposed so a caller can
    /// log/verify which frame is in play rather than assume one.
    pub fn base_from_world(&self) -> Isometry3<f64> {
        self.chain.base_from_world()
    }
}

/// Walk from `base` down the unique revolute-bearing path and return the link
/// reached after exactly [`ARM_DOF`] revolute joints: the SRS wrist (tip). The
/// arm is serial until the wrist, so at each step exactly one child still leads
/// to a revolute joint; a fixed sensor branch or the (prismatic) gripper is
/// skipped, and a genuine fork (two revolute branches) is rejected as not a
/// single SRS arm.
fn find_srs_tip(tree: &Tree, base: usize) -> Result<usize, SrsError> {
    let mut link = base;
    let mut revolute = 0;
    while revolute < ARM_DOF {
        let onward: Vec<usize> = tree
            .children_of(link)
            .iter()
            .copied()
            .filter(|&j| {
                matches!(tree.joint(j).kind, JointKind::Revolute { .. })
                    || tree.subtree_has_revolute(tree.joint(j).child)
            })
            .collect();
        let [joint] = onward[..] else {
            return Err(SrsError::NotSrsArm(if onward.is_empty() {
                format!(
                    "chain from base reaches only {revolute} revolute joints; \
                     a 7-DOF SRS arm needs {ARM_DOF}"
                )
            } else {
                format!(
                    "ambiguous arm: {} revolute-bearing branches share one link; \
                     not a single SRS chain",
                    onward.len()
                )
            }));
        };
        if matches!(tree.joint(joint).kind, JointKind::Revolute { .. }) {
            revolute += 1;
        }
        link = tree.joint(joint).child;
    }
    Ok(link)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nalgebra::{Point3, Vector3};

    fn fk() -> ForwardKinematics {
        crate::test_support::v1_fk("left")
    }

    #[test]
    fn loads_seven_revolute_chain() {
        let fk = fk();
        let posed = fk.at(&[0.0; ARM_DOF]);
        for i in 0..ARM_DOF {
            let n = posed.axis_base(i).norm();
            assert!((n - 1.0).abs() < 1e-9, "joint {i} axis not unit: {n}");
        }
    }

    #[test]
    fn point_world_jacobian_matches_finite_difference() {
        let h = 1e-6;
        let configs: [JointVec; 3] = [
            [0.3, -0.2, 0.5, 0.4, -0.6, 0.2, 0.1],
            [-0.5, 0.4, -0.3, 0.8, 0.5, -0.4, 0.7],
            [0.1, 0.1, 0.1, 0.3, 0.1, 0.1, 0.1],
        ];
        // A fixed offset in each link's frame, so the same material point is tracked
        // across the perturbed configurations.
        let offset = Point3::new(0.05, -0.03, 0.04);
        for side in ["left", "right"] {
            let fk = crate::test_support::v1_fk(side);
            for q in configs {
                for segment in 0..ARM_DOF {
                    let point = fk.at(&q).link_pose_world(segment) * offset;
                    let cols = fk.at(&q).point_world_jacobian(&point, segment);
                    for j in 0..ARM_DOF {
                        let mut qp = q;
                        let mut qm = q;
                        qp[j] += h;
                        qm[j] -= h;
                        let pp = fk.at(&qp).link_pose_world(segment) * offset;
                        let pm = fk.at(&qm).link_pose_world(segment) * offset;
                        let fd = (pp.coords - pm.coords) / (2.0 * h);
                        // For j > segment the column is zero and the point does not
                        // move (a distal joint), so both sides are ~0.
                        assert!(
                            (cols[j] - fd).norm() < 1e-5,
                            "{side} segment {segment} joint {j} off by {}",
                            (cols[j] - fd).norm()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn home_ee_is_above_shoulder() {
        let fk = fk();
        let ee = fk.at(&[0.0; ARM_DOF]).ee_pose();
        // At home the wrist center sits at ~(0, 0.436, 0.1225) in base frame.
        // Mainly a smoke check that the base transform applied.
        let w = ee.translation.vector;
        assert!(
            (w - Vector3::new(0.0, 0.436, 0.1225)).norm() < 1e-3,
            "home EE {w:?} not at expected wrist center",
        );
    }

    #[test]
    fn link_names_follow_fixture_naming() {
        let fk = fk();
        let posed = fk.at(&[0.0; ARM_DOF]);
        for i in 0..ARM_DOF {
            let expected = format!("openarm_left_link{}", i + 1);
            assert_eq!(posed.link_name(i), expected);
        }
    }

    #[test]
    fn last_link_world_pose_matches_ee_pose_via_mount() {
        // link_pose_world(6) is the tip link's world pose; ee_pose is the same
        // pose re-expressed in the base frame, so they must agree through the
        // fixed mount transform.
        let fk = fk();
        let q = [0.3, -0.4, 0.5, 0.6, -0.2, 0.1, 0.7];
        let posed = fk.at(&q);
        let via_mount = posed.base_from_world() * posed.link_pose_world(ARM_DOF - 1);
        let ee = posed.ee_pose();
        assert!((via_mount.translation.vector - ee.translation.vector).norm() < 1e-12);
        assert!(via_mount.rotation.angle_to(&ee.rotation) < 1e-12);
    }

    #[test]
    fn left_and_right_first_links_are_mirrored_in_world() {
        // Both chains share the URDF root frame: at home, the two shoulder
        // (link1) origins must mirror across the XZ plane at the mount offsets.
        let left = crate::test_support::v1_fk("left");
        let right = crate::test_support::v1_fk("right");
        let l = left
            .at(&[0.0; ARM_DOF])
            .link_pose_world(0)
            .translation
            .vector;
        let r = right
            .at(&[0.0; ARM_DOF])
            .link_pose_world(0)
            .translation
            .vector;
        assert!(
            (l - Vector3::new(0.0, 0.0935, 0.698)).norm() < 1e-6,
            "left shoulder at {l:?}"
        );
        assert!(
            (r - Vector3::new(0.0, -0.0935, 0.698)).norm() < 1e-6,
            "right shoulder at {r:?}"
        );
    }

    #[test]
    fn rejects_urdf_missing_arm_links() {
        // A URDF without the arm links must Err, not panic.
        let urdf = r#"<?xml version="1.0"?><robot name="x"><link name="world"/></robot>"#;
        assert!(ForwardKinematics::from_urdf(urdf, "openarm_left_link0").is_err());
    }

    #[test]
    fn rejects_malformed_urdf() {
        assert!(ForwardKinematics::from_urdf("not even xml", "a").is_err());
    }

    #[test]
    fn rejects_chain_without_seven_revolute_joints() {
        // A prismatic-only arm must Err: walking out from the base finds no
        // revolute joints to reach the wrist, so it is not a 7-DOF SRS arm. (A
        // prismatic joint *interspersed* among the 7 is caught instead by the
        // chain's joint count, which sees eight movable joints on the path.)
        let urdf = r#"<?xml version="1.0"?><robot name="x">
          <link name="base"/><link name="tip"/>
          <joint name="slide" type="prismatic">
            <parent link="base"/><child link="tip"/>
            <axis xyz="0 0 1"/><origin xyz="0 0 0"/>
            <limit lower="0" upper="1" effort="1" velocity="1"/>
          </joint>
        </robot>"#;
        let err = match ForwardKinematics::from_urdf(urdf, "base") {
            Ok(_) => panic!("expected Err for a prismatic joint"),
            Err(e) => e,
        };
        assert!(
            matches!(&err, SrsError::NotSrsArm(_)),
            "unexpected error: {err}"
        );
    }
}
