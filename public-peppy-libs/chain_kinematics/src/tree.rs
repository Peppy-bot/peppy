//! The URDF link/joint tree: an arena of links and joints, and the walks over
//! it that [`crate::Chain`] needs.
//!
//! A URDF is a tree of links joined by joints. This flattens it into an arena -
//! links and joints in `Vec`s, adjacency by index - and computes world
//! transforms by composing `parent * origin * joint_motion` down a path. That is
//! the whole of it: no interior mutability, no transform cache to invalidate, no
//! shared ownership. A [`Chain`] is plain data, so forward kinematics is a pure
//! function of a configuration and posing one can never race a read of another.
//!
//! Nothing here knows what a robot is, or how many joints one has. It answers
//! "where is every frame of this linkage at this configuration", for any serial
//! path through any URDF; whatever rule picks the path belongs to the caller.
//!
//! Conventions follow the URDF specification, so a chain built here agrees with
//! any other conforming reader: a joint's `origin` is `translation(xyz)` composed
//! with the extrinsic roll-pitch-yaw of `rpy`, the axis is normalised, and a
//! revolute joint contributes a pure rotation about that axis in the joint frame.

use std::collections::{HashMap, VecDeque};

use nalgebra::{Isometry3, Matrix3, Translation3, Unit, UnitQuaternion, Vector3};

/// How a joint moves, and about what. Prismatic joints are carried rather than
/// rejected here so the layer above can say *why* it will not accept one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JointKind {
    Fixed,
    Revolute { axis: Unit<Vector3<f64>> },
    Prismatic { axis: Unit<Vector3<f64>> },
}

impl JointKind {
    /// True for anything that moves, i.e. anything that needs a joint value.
    pub fn is_movable(self) -> bool {
        !matches!(self, JointKind::Fixed)
    }

    /// The joint's own contribution at value `q`, in its origin frame.
    pub(crate) fn motion(self, q: f64) -> Isometry3<f64> {
        match self {
            JointKind::Fixed => Isometry3::identity(),
            JointKind::Revolute { axis } => Isometry3::from_parts(
                Translation3::new(0.0, 0.0, 0.0),
                UnitQuaternion::from_axis_angle(&axis, q),
            ),
            JointKind::Prismatic { axis } => Isometry3::from_parts(
                Translation3::from(axis.into_inner() * q),
                UnitQuaternion::identity(),
            ),
        }
    }
}

/// One joint: where it sits on its parent, how it moves, and what it carries.
#[derive(Debug)]
pub struct Joint {
    pub name: String,
    pub child: usize,
    pub origin: Isometry3<f64>,
    pub kind: JointKind,
    /// `None` when the URDF declares no usable range (an absent `<limit>` leaves
    /// `lower == upper == 0`, and a continuous joint has no range by definition).
    pub limit: Option<(f64, f64)>,
    /// The joint this one's position is bound to by a URDF `<mimic>`, if any.
    /// Carried rather than resolved: a coupling below the tip is another
    /// mechanism's business, and only [`crate::Chain`] knows where its tip is.
    pub mimic: Option<String>,
}

/// One link's rigid-body data, in the link frame. Links carry no pose of their
/// own: a link's frame *is* its parent joint's frame.
#[derive(Debug)]
pub struct Link {
    pub name: String,
    pub mass: f64,
    pub com: Vector3<f64>,
    /// About the COM, rotated out of the inertial's own `rpy` into the link frame.
    pub inertia: Matrix3<f64>,
}

/// A parsed URDF: links, joints, and the adjacency between them.
#[derive(Debug)]
pub struct Tree {
    links: Vec<Link>,
    joints: Vec<Joint>,
    /// Link index -> the joint that moves it, `None` for the root.
    parent_joint: Vec<Option<usize>>,
    /// Link index -> the joints hanging off it.
    children: Vec<Vec<usize>>,
    /// Joint index -> the link it hangs off.
    parents: Vec<usize>,
    by_name: HashMap<String, usize>,
    root: usize,
}

impl Tree {
    /// Build from a parsed URDF. Errors when the links and joints do not form a
    /// single tree, which every later assumption depends on.
    pub fn from_robot(robot: &urdf_rs::Robot) -> Result<Self, String> {
        let mut by_name = HashMap::with_capacity(robot.links.len());
        let links: Vec<Link> = robot
            .links
            .iter()
            .enumerate()
            .map(|(i, l)| {
                by_name.insert(l.name.clone(), i);
                let origin = pose_of(&l.inertial.origin);
                let r = *origin.rotation.to_rotation_matrix().matrix();
                Link {
                    name: l.name.clone(),
                    mass: l.inertial.mass.value,
                    com: origin.translation.vector,
                    // Rotated out of the inertial's own rpy so a non-identity
                    // inertial frame is carried rather than silently dropped.
                    inertia: r * inertia_of(&l.inertial.inertia) * r.transpose(),
                }
            })
            .collect();
        if links.len() != by_name.len() {
            return Err("URDF has two links with the same name".into());
        }

        let mut parent_joint = vec![None; links.len()];
        let mut children = vec![Vec::new(); links.len()];
        let mut joints = Vec::with_capacity(robot.joints.len());
        let mut parents = Vec::with_capacity(robot.joints.len());
        for j in &robot.joints {
            let parent = *by_name
                .get(&j.parent.link)
                .ok_or_else(|| format!("joint '{}' names an absent parent link", j.name))?;
            let child = *by_name
                .get(&j.child.link)
                .ok_or_else(|| format!("joint '{}' names an absent child link", j.name))?;
            let idx = joints.len();
            if parent_joint[child].is_some() {
                return Err(format!(
                    "link '{}' has two parent joints: the URDF is not a tree",
                    links[child].name
                ));
            }
            parent_joint[child] = Some(idx);
            children[parent].push(idx);
            parents.push(parent);
            joints.push(Joint {
                name: j.name.clone(),
                child,
                origin: pose_of(&j.origin),
                kind: kind_of(j)?,
                // An absent <limit> defaults both bounds to zero; a continuous
                // joint is unbounded. Either way there is no range to report.
                limit: ((j.limit.upper - j.limit.lower) != 0.0)
                    .then_some((j.limit.lower, j.limit.upper)),
                mimic: j.mimic.as_ref().map(|m| m.joint.clone()),
            });
        }

        let roots: Vec<usize> = (0..links.len())
            .filter(|&i| parent_joint[i].is_none())
            .collect();
        let [root] = roots[..] else {
            return Err(format!(
                "URDF must have exactly one root link, found {}",
                roots.len()
            ));
        };
        // One root is not enough to make a tree: a set of links joined in a ring
        // has no root of its own, so a URDF carrying one alongside a real tree
        // still presents exactly one. Every walk here follows `parent_joint`
        // upward and would circle such a ring forever, so it is refused here
        // rather than discovered as a hang.
        if let Some(link) = unreached_from(&children, &joints, root, links.len()) {
            return Err(format!(
                "link '{}' is not reachable from root link '{}': the URDF's links \
                 and joints do not form a single tree",
                links[link].name, links[root].name
            ));
        }

        Ok(Self {
            links,
            joints,
            parent_joint,
            children,
            parents,
            by_name,
            root,
        })
    }

    pub fn link_index(&self, name: &str) -> Option<usize> {
        self.by_name.get(name).copied()
    }

    pub fn link(&self, i: usize) -> &Link {
        &self.links[i]
    }

    /// How many links the URDF declares: the range every link index falls in.
    pub fn link_count(&self) -> usize {
        self.links.len()
    }

    pub fn joint(&self, i: usize) -> &Joint {
        &self.joints[i]
    }

    /// Every joint in the URDF, on this chain or not: what a whole-model rule
    /// such as a `<mimic>` coupling has to be checked against.
    pub fn joints(&self) -> &[Joint] {
        &self.joints
    }

    pub fn children_of(&self, link: usize) -> &[usize] {
        &self.children[link]
    }

    /// Joint indices from the root down to `link`, in order. Empty for the root.
    pub fn path_to(&self, link: usize) -> Vec<usize> {
        let mut path = Vec::new();
        let mut at = link;
        while let Some(j) = self.parent_joint[at] {
            path.push(j);
            at = self.parent_of(j);
        }
        path.reverse();
        path
    }

    /// The link a joint hangs off.
    fn parent_of(&self, joint: usize) -> usize {
        self.parents[joint]
    }

    /// Every link strictly below `link`, in breadth-first order, paired with its
    /// transform relative to `link` at the given configuration of the joints
    /// between them. `value` supplies each joint's position; distal joints a
    /// caller does not name are frozen at zero.
    pub fn subtree_from(
        &self,
        link: usize,
        value: &dyn Fn(usize) -> f64,
    ) -> Vec<(usize, Isometry3<f64>)> {
        let mut out = Vec::new();
        let mut queue = VecDeque::from([(link, Isometry3::identity())]);
        while let Some((at, here)) = queue.pop_front() {
            for &j in &self.children[at] {
                let joint = &self.joints[j];
                let below = here * joint.origin * joint.kind.motion(value(j));
                out.push((joint.child, below));
                queue.push_back((joint.child, below));
            }
        }
        out
    }

    /// The fixed transforms from `from` down to the link named `to`, or `None`
    /// when it is absent from that subtree or is reached through a joint that
    /// moves - in which case the offset would not be fixed at all.
    pub fn fixed_path_to(&self, from: usize, to: &str) -> Option<Isometry3<f64>> {
        for &j in &self.children[from] {
            let joint = &self.joints[j];
            if joint.kind.is_movable() {
                continue;
            }
            if self.links[joint.child].name == to {
                return Some(joint.origin);
            }
            if let Some(rest) = self.fixed_path_to(joint.child, to) {
                return Some(joint.origin * rest);
            }
        }
        None
    }

    /// Whether any joint at or below `link` rotates: the test that distinguishes
    /// the branch continuing an arm from a fixed mount or a gripper.
    pub fn subtree_has_revolute(&self, link: usize) -> bool {
        self.children[link].iter().any(|&j| {
            matches!(self.joints[j].kind, JointKind::Revolute { .. })
                || self.subtree_has_revolute(self.joints[j].child)
        })
    }

    pub fn root(&self) -> usize {
        self.root
    }
}

/// The first link no walk down from `root` reaches, if any: proof that the
/// links and joints are not one connected tree.
fn unreached_from(
    children: &[Vec<usize>],
    joints: &[Joint],
    root: usize,
    links: usize,
) -> Option<usize> {
    let mut seen = vec![false; links];
    seen[root] = true;
    let mut queue = VecDeque::from([root]);
    while let Some(at) = queue.pop_front() {
        for &j in &children[at] {
            let child = joints[j].child;
            if !std::mem::replace(&mut seen[child], true) {
                queue.push_back(child);
            }
        }
    }
    seen.iter().position(|reached| !reached)
}

/// URDF `<origin>` as a transform: `translation(xyz)` composed with the extrinsic
/// roll-pitch-yaw of `rpy`.
fn pose_of(p: &urdf_rs::Pose) -> Isometry3<f64> {
    Isometry3::from_parts(
        Translation3::new(p.xyz[0], p.xyz[1], p.xyz[2]),
        UnitQuaternion::from_euler_angles(p.rpy[0], p.rpy[1], p.rpy[2]),
    )
}

/// URDF `<inertia>`'s six independent entries as a symmetric matrix.
fn inertia_of(i: &urdf_rs::Inertia) -> Matrix3<f64> {
    Matrix3::new(
        i.ixx, i.ixy, i.ixz, i.ixy, i.iyy, i.iyz, i.ixz, i.iyz, i.izz,
    )
}

/// The joint's kind, or a refusal for the kinds this library cannot pose.
///
/// A floating, planar or spherical joint occupies more than one configuration
/// slot, so treating it as anything simpler silently welds or misplaces every
/// link below it. Refused for the whole model, not just the chain: a multi-slot
/// joint anywhere makes the tree's single-value-per-joint contract a lie.
fn kind_of(j: &urdf_rs::Joint) -> Result<JointKind, String> {
    let axis = || Unit::new_normalize(Vector3::new(j.axis.xyz[0], j.axis.xyz[1], j.axis.xyz[2]));
    match &j.joint_type {
        urdf_rs::JointType::Revolute | urdf_rs::JointType::Continuous => {
            Ok(JointKind::Revolute { axis: axis() })
        }
        urdf_rs::JointType::Prismatic => Ok(JointKind::Prismatic { axis: axis() }),
        urdf_rs::JointType::Fixed => Ok(JointKind::Fixed),
        other => Err(format!(
            "joint '{}' has type {other:?}, which needs more than one configuration \
             value; supported joint types are revolute, continuous, prismatic and fixed",
            j.name
        )),
    }
}
