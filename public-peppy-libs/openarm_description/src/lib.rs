//! The OpenArm robot descriptions: the single embedded source of truth for the URDFs
//! and their collision meshes, so nodes no longer each ship their own copy.
//!
//! Two hardware generations are carried, selected by [`HardwareVersion`]:
//! - [`HardwareVersion::V1`]: OpenArm v1.0 (`openarm_v10`).
//! - [`HardwareVersion::V2`]: OpenArm v2.0 (`openarm_v20`).
//!
//! - [`HardwareVersion::urdf`] returns the bundled URDF string (mechanical joint limits,
//!   as vendored).
//! - [`HardwareVersion::write_meshes_to`] (feature `meshes`) materializes the embedded
//!   collision meshes for the file-based bimanual collision builder and for any non-Rust
//!   consumer (the sim) via the `emit_meshes` binary.
//! - [`HardwareVersion::elbow_singularity_floor_rad`] / [`HardwareVersion::elbow_joint_index`]
//!   describe the elbow control margin the kinematics consumer applies (see the method docs).
//! - [`HardwareVersion::joint_limits`] resolves one side's per-joint position limits from the
//!   bundled URDF with that margin applied: the single clamp source for every node that
//!   produces joint commands.
//! - [`HardwareVersion::base_link`] names one side's chain base link in the bundled URDF:
//!   the single source for the link a kinematics chain is walked out from.
//! - [`HardwareVersion::tcp_link`] names one side's tool-center-point frame in the bundled
//!   URDF, so poses are commanded and reported at the grasp point (see the method docs for
//!   the frame convention).
//!
//! Pure data: this crate carries no solver dependency. A consumer that wants a kinematic
//! model builds it from these, e.g.
//!
//! ```text
//! srs_model::Arm::from_urdf(v.urdf(), v.base_link(side))?
//!     .with_lower_floor(v.elbow_joint_index(), v.elbow_singularity_floor_rad())
//!     .with_tool_link(v.tcp_link(side))?
//! ```
//!
//! so the description stays reusable by any consumer (a viz tool, a sim bridge) without
//! pulling a solver in. A consumer that only needs dynamics (an arm driver's gravity
//! feedforward) can skip the tool: it moves the end-effector frame, not the masses.

use std::fmt;
use std::str::FromStr;

/// Joints per arm (j1..j7) in both generations.
pub const ARM_DOF: usize = 7;

/// An arm side of the bimanual robot, selecting the `openarm_left_*` or
/// `openarm_right_*` chain in the bundled URDF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    fn urdf_prefix(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
        }
    }
}

/// An OpenArm hardware generation. A node parses its `hardware_version` parameter into
/// this once (parse, don't validate) and then reads the bundled description through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareVersion {
    /// OpenArm v1.0 (`openarm_v10`): prismatic parallel-jaw gripper.
    V1,
    /// OpenArm v2.0 (`openarm_v20`): reoriented arm frames, revolute pinch gripper.
    V2,
}

impl HardwareVersion {
    /// The bundled URDF for this generation. Mechanical joint limits are as vendored
    /// upstream (enactic/openarm_description); the elbow singularity margin lives in
    /// [`Self::elbow_singularity_floor_rad`], not in the file.
    pub fn urdf(self) -> &'static str {
        match self {
            Self::V1 => include_str!("../assets/openarm_v10.urdf"),
            Self::V2 => include_str!("../assets/openarm_v20.urdf"),
        }
    }

    /// Lower bound (rad) a kinematics consumer should impose on the elbow (j4) beyond its
    /// mechanical `0.0`. At full extension the arm is at the straight-arm singularity,
    /// where a closed-form arm-angle IK is undefined; this floor holds the redundancy
    /// reference off it. It is a control margin, not a mechanical limit, so it lives here
    /// rather than in the URDF. Both generations share the value today; returning it per
    /// version keeps a future divergence a data change, not a code change.
    pub fn elbow_singularity_floor_rad(self) -> f64 {
        match self {
            Self::V1 | Self::V2 => 0.05,
        }
    }

    /// Index of the elbow joint (j4) in the 0-based `j1..j7` joint vector, for applying
    /// [`Self::elbow_singularity_floor_rad`]. j4 is the elbow in both generations.
    pub fn elbow_joint_index(self) -> usize {
        match self {
            Self::V1 | Self::V2 => 3,
        }
    }

    /// Per-joint `[lower, upper]` position limits (rad) for one arm side, j1..j7, from
    /// the bundled URDF with [`Self::elbow_singularity_floor_rad`] applied to the elbow.
    /// This is the clamp range a command-producing node (operator panel, leader arm)
    /// applies before streaming; the backbone and the arm clamp again on their side. Panics
    /// only if the bundled URDF is malformed, which this crate's tests rule out.
    pub fn joint_limits(self, side: Side) -> [[f64; 2]; ARM_DOF] {
        let robot = urdf_rs::read_from_string(self.urdf()).expect("bundled URDF must parse");
        let elbow = self.elbow_joint_index();
        let floor = self.elbow_singularity_floor_rad();
        std::array::from_fn(|i| {
            let name = format!("openarm_{}_joint{}", side.urdf_prefix(), i + 1);
            let joint = robot
                .joints
                .iter()
                .find(|j| j.name == name)
                .unwrap_or_else(|| panic!("bundled URDF missing joint {name}"));
            let lower = if i == elbow {
                joint.limit.lower.max(floor)
            } else {
                joint.limit.lower
            };
            [lower, joint.limit.upper]
        })
    }

    /// The chain base link naming one side's 7-DOF arm in the bundled URDF: the link
    /// the kinematics chain is walked out from. The generations name it differently
    /// (v1 `openarm_{side}_link0`; v2 folded the mount roll into the chain and named
    /// it `openarm_{side}_base_link`), so it is a property of the generation's data,
    /// resolved here rather than configured per node.
    pub fn base_link(self, side: Side) -> &'static str {
        match (self, side) {
            (Self::V1, Side::Left) => "openarm_left_link0",
            (Self::V1, Side::Right) => "openarm_right_link0",
            (Self::V2, Side::Left) => "openarm_left_base_link",
            (Self::V2, Side::Right) => "openarm_right_base_link",
        }
    }

    /// The tool-center-point frame naming one side's grasp point in the bundled URDF:
    /// the link a kinematics consumer mounts via `with_tool_link`, mirroring how
    /// [`Self::base_link`] names the chain root. The frame itself (a fixed joint off
    /// the chain tip) lives in the URDF with the rest of the robot's geometry; this
    /// resolves its per-generation name.
    ///
    /// The point is on the gripping face, on the jaw closing axis midway between the
    /// pads, located by the jaw's own kinematics:
    ///
    /// - v1's jaws are **prismatic**, so the pads stay parallel at every opening and an
    ///   object meets the whole face. The point is the face centre.
    /// - v2's jaws are **revolute**, so the pads splay open and the face tapers. An object
    ///   wedges where the gap is narrowest, which measures the same (the pad's proximal
    ///   edge) for every object from 10 mm to the jaw's 65 mm limit, so the point is that
    ///   pinch line rather than the face centre 5 mm ahead of it.
    ///
    /// Frame convention, both generations: `+z` out of the gripper along its approach
    /// direction, `y` on the jaw closing axis. The generations mount their grippers along
    /// opposite tip axes (v1 along tip `+z`, v2 along tip `-z`), so v2's frame carries a
    /// half turn about `y` in the URDF; a consumer writing a top-down grasp need not know
    /// which generation it drives.
    pub fn tcp_link(self, side: Side) -> &'static str {
        match (self, side) {
            (Self::V1 | Self::V2, Side::Left) => "openarm_left_tcp",
            (Self::V1 | Self::V2, Side::Right) => "openarm_right_tcp",
        }
    }
}
impl fmt::Display for HardwareVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::V1 => "v1",
            Self::V2 => "v2",
        })
    }
}

impl FromStr for HardwareVersion {
    type Err = UnknownHardwareVersion;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "v1" | "V1" => Ok(Self::V1),
            "v2" | "V2" => Ok(Self::V2),
            other => Err(UnknownHardwareVersion(other.to_owned())),
        }
    }
}

/// Returned when a `hardware_version` string is neither `v1` nor `v2`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownHardwareVersion(pub String);

impl fmt::Display for UnknownHardwareVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown hardware_version '{}' (expected 'v1' or 'v2')",
            self.0
        )
    }
}

impl std::error::Error for UnknownHardwareVersion {}

/// The torso mesh, shared by both generations (the bimanual stand is unchanged).
/// Embedded once and referenced from both mesh lists; a test asserts the v2.0
/// asset file stays byte-identical.
#[cfg(feature = "meshes")]
const TORSO_MESH: &[u8] = include_bytes!("../assets/meshes/body_link0_symp.stl");

/// The bundled collision meshes as `(file name, bytes)`. The bimanual collision builder
/// resolves the URDF's `package://` mesh refs by file name against a meshes directory, so
/// [`HardwareVersion::write_meshes_to`] lays these down under their bare names.
#[cfg(feature = "meshes")]
const V1_MESHES: &[(&str, &[u8])] = &[
    ("body_link0_symp.stl", TORSO_MESH),
    ("finger.stl", include_bytes!("../assets/meshes/finger.stl")),
    (
        "link0_symp.stl",
        include_bytes!("../assets/meshes/link0_symp.stl"),
    ),
    (
        "link1_symp.stl",
        include_bytes!("../assets/meshes/link1_symp.stl"),
    ),
    (
        "link2_symp.stl",
        include_bytes!("../assets/meshes/link2_symp.stl"),
    ),
    (
        "link3_symp.stl",
        include_bytes!("../assets/meshes/link3_symp.stl"),
    ),
    (
        "link4_symp.stl",
        include_bytes!("../assets/meshes/link4_symp.stl"),
    ),
    (
        "link5_symp.stl",
        include_bytes!("../assets/meshes/link5_symp.stl"),
    ),
    (
        "link6_symp.stl",
        include_bytes!("../assets/meshes/link6_symp.stl"),
    ),
    (
        "link7_symp.stl",
        include_bytes!("../assets/meshes/link7_symp.stl"),
    ),
];

/// OpenArm v2.0 collision meshes: reoriented arm links (`base_link`, `link1..link6`) and
/// the revolute pinch gripper (`ee_base_link`, `finger_inner`, `finger_outer`), plus the
/// shared torso proxy mesh.
#[cfg(feature = "meshes")]
const V2_MESHES: &[(&str, &[u8])] = &[
    ("body_link0_symp.stl", TORSO_MESH),
    (
        "base_link.stl",
        include_bytes!("../assets/meshes_v20/base_link.stl"),
    ),
    (
        "link1.stl",
        include_bytes!("../assets/meshes_v20/link1.stl"),
    ),
    (
        "link2.stl",
        include_bytes!("../assets/meshes_v20/link2.stl"),
    ),
    (
        "link3.stl",
        include_bytes!("../assets/meshes_v20/link3.stl"),
    ),
    (
        "link4.stl",
        include_bytes!("../assets/meshes_v20/link4.stl"),
    ),
    (
        "link5.stl",
        include_bytes!("../assets/meshes_v20/link5.stl"),
    ),
    (
        "link6.stl",
        include_bytes!("../assets/meshes_v20/link6.stl"),
    ),
    (
        "ee_base_link.stl",
        include_bytes!("../assets/meshes_v20/ee_base_link.stl"),
    ),
    (
        "finger_inner.stl",
        include_bytes!("../assets/meshes_v20/finger_inner.stl"),
    ),
    (
        "finger_outer.stl",
        include_bytes!("../assets/meshes_v20/finger_outer.stl"),
    ),
];

#[cfg(feature = "meshes")]
impl HardwareVersion {
    /// The embedded collision meshes for this generation.
    fn meshes(self) -> &'static [(&'static str, &'static [u8])] {
        match self {
            Self::V1 => V1_MESHES,
            Self::V2 => V2_MESHES,
        }
    }

    /// Write this generation's embedded collision meshes into `dir` (created if absent).
    /// Materializing them lets the file-based collision builder, and the sim via the
    /// `emit_meshes` binary, consume assets that travel with this crate instead of a
    /// per-node or per-image vendored copy.
    pub fn write_meshes_to(self, dir: &std::path::Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        for (name, bytes) in self.meshes() {
            std::fs::write(dir.join(name), bytes)?;
        }
        Ok(())
    }
}

/// A whole-arm joint posture (rad, j1..j7) in right-arm convention; the left
/// arm takes its [`mirror`].
pub type JointPosture = [f64; ARM_DOF];

/// The right-arm Ready posture: elbows bent, hands forward, prepared to work.
/// The single source both the executor (backbone) and any gesture authoring
/// anchor on; in-limit for both generations, pinned by test.
pub const READY_R: JointPosture = [0.15, 0.40, -0.48, 0.95, 0.0, 0.0, 0.0];

/// The right-arm Home posture: hanging at rest with the elbow held exactly at
/// its singularity floor, the safe posture to power down from. In-limit for
/// both generations, pinned by test.
pub const HOME_R: JointPosture = [0.0, 0.0, 0.0, 0.05, 0.0, 0.0, 0.0];

/// Mirror a right-arm posture onto the left arm: j1..j3 flip sign, the elbow
/// and wrist do not. Exported for right-convention gesture authoring; for the
/// postures themselves, [`ready`] and [`home`] resolve a side directly.
pub const fn mirror(q: JointPosture) -> JointPosture {
    [-q[0], -q[1], -q[2], q[3], q[4], q[5], q[6]]
}

/// One side's Ready posture.
pub const fn ready(side: Side) -> JointPosture {
    for_side(side, READY_R)
}

/// One side's Home posture.
pub const fn home(side: Side) -> JointPosture {
    for_side(side, HOME_R)
}

const fn for_side(side: Side, right: JointPosture) -> JointPosture {
    match side {
        Side::Left => mirror(right),
        Side::Right => right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(v: HardwareVersion) -> urdf_rs::Robot {
        urdf_rs::read_from_string(v.urdf()).expect("bundled URDF parses")
    }

    #[test]
    fn base_link_names_the_exact_link_each_urdf_carries() {
        // Pin the exact per-side base link (v1 `link0`, v2 `base_link`, after v2 folded
        // the ±90° mount roll into the arm chain), and separately confirm that name is a
        // link the bundled URDF actually carries. The expected names are explicit, not
        // derived from the mapping under test, so a wrong mapping onto some other
        // existing link cannot pass.
        let cases = [
            (HardwareVersion::V1, Side::Left, "openarm_left_link0"),
            (HardwareVersion::V1, Side::Right, "openarm_right_link0"),
            (HardwareVersion::V2, Side::Left, "openarm_left_base_link"),
            (HardwareVersion::V2, Side::Right, "openarm_right_base_link"),
        ];
        for (v, side, expected) in cases {
            assert_eq!(v.base_link(side), expected, "{v} {side:?}: base link name");
            assert!(
                parsed(v).links.iter().any(|l| l.name == expected),
                "{v}: bundled URDF missing link {expected}"
            );
        }
    }

    #[test]
    fn both_generations_share_the_bimanual_torso() {
        for v in [HardwareVersion::V1, HardwareVersion::V2] {
            let robot = parsed(v);
            assert!(
                robot.links.iter().any(|l| l.name == "openarm_body_link0"),
                "{v}: missing shared torso link"
            );
        }
    }

    #[cfg(feature = "meshes")]
    #[test]
    fn v2_torso_asset_stays_byte_identical_to_the_shared_embed() {
        let v2_file: &[u8] = include_bytes!("../assets/meshes_v20/body_link0_symp.stl");
        assert_eq!(TORSO_MESH, v2_file);
    }

    #[test]
    fn keeps_the_mechanical_elbow_limit_in_both() {
        // The file carries the vendored `0.0`; the singularity margin is a control policy
        // the consumer applies (elbow_singularity_floor_rad), not baked into the data.
        let cases = [
            (
                HardwareVersion::V1,
                ["openarm_left_joint4", "openarm_right_joint4"],
            ),
            (
                HardwareVersion::V2,
                ["openarm_left_joint4", "openarm_right_joint4"],
            ),
        ];
        for (v, elbows) in cases {
            let robot = parsed(v);
            for elbow in elbows {
                let joint = robot
                    .joints
                    .iter()
                    .find(|j| j.name == elbow)
                    .unwrap_or_else(|| panic!("{v}: missing elbow joint {elbow}"));
                assert_eq!(
                    joint.limit.lower, 0.0,
                    "{v}: {elbow} lower limit is mechanical"
                );
            }
        }
    }

    #[test]
    fn v2_widened_the_shoulder_pitch_limit() {
        // v1 joint2 is symmetric ±1.7453; v2 widened it to an asymmetric range with a
        // ~3.3161 rad reach. Guard the magnitude so a stale vendored URDF is caught.
        let j2 = |v: HardwareVersion| {
            parsed(v)
                .joints
                .into_iter()
                .find(|j| j.name == "openarm_left_joint2")
                .expect("joint2 present")
                .limit
        };
        let v1 = j2(HardwareVersion::V1);
        let v2 = j2(HardwareVersion::V2);
        assert!(
            (v1.upper - v1.lower) < 3.6,
            "v1 joint2 is the symmetric range"
        );
        assert!(
            (v2.upper - v2.lower) > 3.4,
            "v2 joint2 widened: got [{}, {}]",
            v2.lower,
            v2.upper
        );
    }

    #[test]
    fn both_postures_sit_inside_every_generation_and_side_limits() {
        for v in [HardwareVersion::V1, HardwareVersion::V2] {
            for side in [Side::Left, Side::Right] {
                let limits = v.joint_limits(side);
                for (name, posture) in
                    [("ready", ready as fn(Side) -> JointPosture), ("home", home)]
                {
                    let q = posture(side);
                    for j in 0..ARM_DOF {
                        assert!(
                            limits[j][0] <= q[j] && q[j] <= limits[j][1],
                            "{v:?} {side:?} {name} j{} = {} outside {:?}",
                            j + 1,
                            q[j],
                            limits[j],
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn home_holds_the_elbow_at_its_floor() {
        for v in [HardwareVersion::V1, HardwareVersion::V2] {
            assert_eq!(
                HOME_R[v.elbow_joint_index()],
                v.elbow_singularity_floor_rad()
            );
        }
    }

    #[test]
    fn mirror_flips_the_shoulder_and_keeps_the_elbow_and_wrist() {
        let m = mirror(READY_R);
        for j in 0..3 {
            assert_eq!(m[j], -READY_R[j]);
        }
        for j in 3..ARM_DOF {
            assert_eq!(m[j], READY_R[j]);
        }
    }

    #[test]
    fn joint_limits_are_well_formed_with_the_elbow_floored() {
        for v in [HardwareVersion::V1, HardwareVersion::V2] {
            let elbow = v.elbow_joint_index();
            for side in [Side::Left, Side::Right] {
                let limits = v.joint_limits(side);
                for (i, &[lo, hi]) in limits.iter().enumerate() {
                    assert!(lo < hi, "{v} {side:?} j{}: range [{lo}, {hi}]", i + 1);
                }
                assert_eq!(
                    limits[elbow][0],
                    v.elbow_singularity_floor_rad(),
                    "{v} {side:?}: elbow lower must be the singularity floor"
                );
            }
        }
    }

    #[test]
    fn joint_limits_match_the_urdf_outside_the_elbow() {
        // Only the elbow lower bound is adjusted; every other bound is the file's.
        for v in [HardwareVersion::V1, HardwareVersion::V2] {
            let robot = parsed(v);
            let elbow = v.elbow_joint_index();
            for (side, prefix) in [(Side::Left, "left"), (Side::Right, "right")] {
                let limits = v.joint_limits(side);
                for (i, &[lo, hi]) in limits.iter().enumerate() {
                    let joint = robot
                        .joints
                        .iter()
                        .find(|j| j.name == format!("openarm_{prefix}_joint{}", i + 1))
                        .expect("joint present");
                    assert_eq!(hi, joint.limit.upper);
                    if i != elbow {
                        assert_eq!(lo, joint.limit.lower);
                    }
                }
            }
        }
    }

    #[test]
    fn hardware_version_round_trips_through_str() {
        for (s, v) in [("v1", HardwareVersion::V1), ("v2", HardwareVersion::V2)] {
            assert_eq!(s.parse::<HardwareVersion>().unwrap(), v);
            assert_eq!(v.to_string(), s);
        }
        assert!("v3".parse::<HardwareVersion>().is_err());
    }

    /// The finger geometry of one side, in the chain-tip frame: where the finger hinges
    /// (the knuckle) and how far the finger reaches past it along each axis.
    #[cfg(feature = "meshes")]
    struct FingerSpan {
        knuckle: [f64; 3],
        min: [f64; 3],
        max: [f64; 3],
        /// The finger's collision vertices, placed in the chain-tip frame with the
        /// jaws closed.
        points: Vec<[f64; 3]>,
    }

    /// Read one finger's geometry straight out of the bundled URDF and meshes, so the
    /// tool transform is checked against the description rather than against itself.
    #[cfg(feature = "meshes")]
    fn finger_span(v: HardwareVersion, side: Side, finger: usize) -> FingerSpan {
        let robot = parsed(v);
        let joint_name = format!("openarm_{}_finger_joint{finger}", side.urdf_prefix());
        let joint = robot
            .joints
            .iter()
            .find(|j| j.name == joint_name)
            .unwrap_or_else(|| panic!("{v}: bundled URDF missing {joint_name}"));
        let knuckle = joint.origin.xyz.0;
        let link = robot
            .links
            .iter()
            .find(|l| l.name == joint.child.link)
            .unwrap_or_else(|| panic!("{v}: bundled URDF missing link {}", joint.child.link));
        let collision = link
            .collision
            .first()
            .unwrap_or_else(|| panic!("{v}: {} has no collision geometry", link.name));
        let urdf_rs::Geometry::Mesh { filename, scale } = &collision.geometry else {
            panic!("{v}: {} collision is not a mesh", link.name);
        };
        // The placement below is translation + scale only, so a rotation on either
        // origin would silently misplace the geometry this test brackets against.
        for (what, rpy) in [
            ("joint", joint.origin.rpy.0),
            ("collision", collision.origin.rpy.0),
        ] {
            assert!(
                rpy.iter().all(|c| c.abs() < 1e-12),
                "{v} {side:?}: finger {what} origin carries a rotation {rpy:?}; teach \
                 finger_span the transform before trusting this test again"
            );
        }
        let scale = scale.map(|s| s.0).unwrap_or([1.0; 3]);
        let file = filename
            .rsplit('/')
            .next()
            .expect("mesh filename has a final component");
        let (_, bytes) = v
            .meshes()
            .iter()
            .find(|(name, _)| *name == file)
            .unwrap_or_else(|| panic!("{v}: {file} is not embedded"));
        let (lo, hi) = stl_bounds(bytes);
        // The mirror in `scale` flips a bound onto the other end, so order after scaling.
        let placed =
            |k: usize, bound: f64| bound * scale[k] + collision.origin.xyz.0[k] + knuckle[k];
        FingerSpan {
            knuckle,
            min: std::array::from_fn(|k| placed(k, lo[k]).min(placed(k, hi[k]))),
            max: std::array::from_fn(|k| placed(k, lo[k]).max(placed(k, hi[k]))),
            points: stl_vertices(bytes)
                .map(|v| {
                    std::array::from_fn(|k| {
                        v[k] * scale[k] + collision.origin.xyz.0[k] + knuckle[k]
                    })
                })
                .collect(),
        }
    }

    /// The closed-jaw gap between two fingers in the `z` slice around `at`: how far
    /// apart their facing surfaces are, negative where the collision meshes overlap.
    /// `None` when neither finger has geometry there.
    #[cfg(feature = "meshes")]
    fn closed_gap_at(a: &FingerSpan, b: &FingerSpan, at: f64, half_slice: f64) -> Option<f64> {
        let slice = |f: &FingerSpan| -> Vec<f64> {
            f.points
                .iter()
                .filter(|p| (p[2] - at).abs() <= half_slice)
                .map(|p| p[1])
                .collect()
        };
        let (ys_a, ys_b) = (slice(a), slice(b));
        if ys_a.is_empty() || ys_b.is_empty() {
            return None;
        }
        // Which finger sits on which side of the closing axis flips between the
        // arms (the URDF mirrors finger 1 across them), so order by the geometry
        // rather than by argument position, or the gap goes negative on one side
        // and a `gap <= tolerance` assertion passes vacuously there.
        let mid = |ys: &[f64]| ys.iter().sum::<f64>() / ys.len() as f64;
        let (lower, upper) = if mid(&ys_a) <= mid(&ys_b) {
            (&ys_a, &ys_b)
        } else {
            (&ys_b, &ys_a)
        };
        let inner_lower = lower.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let inner_upper = upper.iter().copied().fold(f64::INFINITY, f64::min);
        Some(inner_upper - inner_lower)
    }

    /// The vertices of a binary STL: an 80-byte header, a triangle count, then 50
    /// bytes per triangle (a normal, three vertices, an attribute count).
    #[cfg(feature = "meshes")]
    fn stl_vertices(bytes: &[u8]) -> impl Iterator<Item = [f64; 3]> + '_ {
        const HEADER: usize = 84;
        const TRIANGLE: usize = 50;
        let count = u32::from_le_bytes(bytes[80..HEADER].try_into().expect("4 bytes")) as usize;
        assert_eq!(
            bytes.len(),
            HEADER + count * TRIANGLE,
            "not a binary STL of {count} triangles"
        );
        (0..count).flat_map(move |tri| {
            (0..3).map(move |vertex| {
                std::array::from_fn(|axis| {
                    let at = HEADER + tri * TRIANGLE + 12 + vertex * 12 + axis * 4;
                    f32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes")) as f64
                })
            })
        })
    }

    /// Per-axis `(min, max)` of a binary STL's vertices.
    #[cfg(feature = "meshes")]
    fn stl_bounds(bytes: &[u8]) -> ([f64; 3], [f64; 3]) {
        let mut lo = [f64::INFINITY; 3];
        let mut hi = [f64::NEG_INFINITY; 3];
        for v in stl_vertices(bytes) {
            for axis in 0..3 {
                lo[axis] = lo[axis].min(v[axis]);
                hi[axis] = hi[axis].max(v[axis]);
            }
        }
        (lo, hi)
    }

    /// The tcp frame as the URDF carries it: the fixed joint's translation and the
    /// rotation matrix its rpy composes to, in the chain-tip frame.
    fn tcp_joint(v: HardwareVersion, side: Side) -> ([f64; 3], [[f64; 3]; 3]) {
        let robot = parsed(v);
        let name = v.tcp_link(side);
        let joint = robot
            .joints
            .iter()
            .find(|j| j.child.link == name)
            .unwrap_or_else(|| panic!("{v}: bundled URDF missing a joint to {name}"));
        assert!(
            matches!(joint.joint_type, urdf_rs::JointType::Fixed),
            "{v} {side:?}: the tcp joint must be fixed"
        );
        let [roll, pitch, yaw] = joint.origin.rpy.0;
        let (sr, cr) = (roll.sin(), roll.cos());
        let (sp, cp) = (pitch.sin(), pitch.cos());
        let (sy, cy) = (yaw.sin(), yaw.cos());
        // URDF rpy is extrinsic XYZ: R = Rz(yaw) Ry(pitch) Rx(roll).
        let r = [
            [cy * cp, cy * sp * sr - sy * cr, cy * sp * cr + sy * sr],
            [sy * cp, sy * sp * sr + cy * cr, sy * sp * cr - cy * sr],
            [-sp, cp * sr, cp * cr],
        ];
        (joint.origin.xyz.0, r)
    }

    /// Gated with its only caller: without the meshes the grasp-frame test that
    /// uses it does not build either.
    #[cfg(feature = "meshes")]
    fn rotate(r: [[f64; 3]; 3], p: [f64; 3]) -> [f64; 3] {
        std::array::from_fn(|i| r[i][0] * p[0] + r[i][1] * p[1] + r[i][2] * p[2])
    }

    #[test]
    fn tcp_link_names_a_fixed_frame_the_urdf_carries() {
        for v in [HardwareVersion::V1, HardwareVersion::V2] {
            for side in [Side::Left, Side::Right] {
                // tcp_joint itself asserts existence and fixity; the two sides must
                // also mirror in y and agree elsewhere, like the arms they hang off.
                let (t, _) = tcp_joint(v, side);
                let (u, _) = tcp_joint(
                    v,
                    match side {
                        Side::Left => Side::Right,
                        Side::Right => Side::Left,
                    },
                );
                assert_eq!(t[0], u[0], "{v}: tcp x differs across sides");
                assert_eq!(t[1], -u[1], "{v}: tcp y must mirror across sides");
                assert_eq!(t[2], u[2], "{v}: tcp z differs across sides");
            }
        }
    }

    #[cfg(feature = "meshes")]
    #[test]
    fn tool_center_point_sits_between_the_knuckle_and_the_fingertip() {
        // Bracketed against the bundled description, not against a second copy of the
        // number: a re-vendored gripper moves the knuckle or the finger and fails here
        // rather than silently leaving the constant describing the old hardware.
        for v in [HardwareVersion::V1, HardwareVersion::V2] {
            for side in [Side::Left, Side::Right] {
                let (translation, rotation) = tcp_joint(v, side);
                let finger = finger_span(v, side, 1);
                let opposing = finger_span(v, side, 2);
                let [x, y, z] = translation;
                // Midway between the two fingers is what "grasp point" means, so pin it
                // to the knuckles rather than to a zero that happens to be right today.
                let midpoint = 0.5 * (finger.knuckle[1] + opposing.knuckle[1]);
                assert!(
                    (y - midpoint).abs() < 1e-9,
                    "{v} {side:?}: tool y {y} is not midway between the fingers ({midpoint})"
                );
                let (near, far) = (
                    finger.knuckle[2],
                    if z < 0.0 {
                        finger.min[2]
                    } else {
                        finger.max[2]
                    },
                );
                assert!(
                    (z - near).abs() < (far - near).abs() && (z - near) * (far - near) > 0.0,
                    "{v} {side:?}: tool z {z} is not between the knuckle {near} and the fingertip {far}"
                );
                assert!(
                    (finger.min[0]..=finger.max[0]).contains(&x),
                    "{v} {side:?}: tool x {x} is outside the finger's {:?}..{:?}",
                    finger.min[0],
                    finger.max[0]
                );
                // On the gripping face, not in the relief behind it or past the tip:
                // with the jaws closed the two pads meet at the tool's own z, which is
                // false everywhere else along the finger.
                let gap = closed_gap_at(&finger, &opposing, z, 0.002)
                    .unwrap_or_else(|| panic!("{v} {side:?}: no finger geometry at z {z}"));
                assert!(
                    gap <= 1e-3,
                    "{v} {side:?}: jaws stand {:.1} mm apart at the tool's z {z}, so it is \
                     not on the gripping face",
                    gap * 1000.0
                );
                // The tool frame's +z is the approach direction: it must point from the
                // knuckle toward the fingertip, which is what makes the two generations
                // interchangeable to a caller despite their opposite mounting axes.
                let approach = rotate(rotation, [0.0, 0.0, 1.0]);
                assert!(
                    approach[2] * (far - near) > 0.0,
                    "{v} {side:?}: tool +z {approach:?} does not point out of the gripper"
                );
                // And the frame's y is the jaw closing axis: the fingers hinge apart
                // along tip y in both URDFs, so tool y must map onto it, not off it.
                let closing = rotate(rotation, [0.0, 1.0, 0.0]);
                assert!(
                    closing[1].abs() > 1.0 - 1e-9,
                    "{v} {side:?}: tool y {closing:?} is off the jaw closing axis"
                );
            }
        }
    }

    #[cfg(feature = "meshes")]
    #[test]
    fn write_meshes_to_lays_down_every_mesh_for_both_generations() {
        for v in [HardwareVersion::V1, HardwareVersion::V2] {
            let dir = std::env::temp_dir().join(format!("openarm_description_meshes_{v}"));
            let _ = std::fs::remove_dir_all(&dir);
            v.write_meshes_to(&dir).expect("materialize meshes");
            for (name, _) in v.meshes() {
                assert!(
                    dir.join(name).is_file(),
                    "{v}: missing materialized mesh {name}"
                );
            }
            std::fs::remove_dir_all(&dir).expect("cleanup");
        }
    }
}
