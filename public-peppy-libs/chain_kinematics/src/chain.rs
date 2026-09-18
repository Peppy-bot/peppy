//! The chain itself: where it starts, where it ends, which joints the caller
//! drives, and the posed view every pose-dependent quantity is read off.

use nalgebra::{Isometry3, Matrix3, Point3, Vector3, Vector6};

use crate::Limit;
use crate::error::ChainError;
use crate::jacobian::Jacobian;
use crate::payload::{Payload, segments_carrying};
use crate::tree::{JointKind, Tree};

/// Which joints along the chain the caller drives, and in what order.
#[derive(Debug, Clone, Copy)]
pub enum JointSelection<'a> {
    /// Every movable joint between base and tip, in the order they appear along
    /// the path.
    PathOrder,
    /// Exactly these joints, in this order - which is the order of `q`. A
    /// movable joint on the path that is not named is frozen at zero and folded
    /// in as a fixed offset.
    Named(&'a [&'a str]),
}

/// Where a chain starts, where it ends, and which joints are actuated.
#[derive(Debug, Clone, Copy)]
pub struct ChainSpec<'a> {
    /// The frame poses are reported in. `None` means the URDF root, and
    /// [`Chain::base_from_world`] is then the identity.
    pub base_link: Option<&'a str>,
    /// The frame poses are commanded and reported at.
    pub tip_link: &'a str,
    pub joints: JointSelection<'a>,
}

/// A serial chain with `N` actuated joints, ready to pose.
///
/// Plain data: no interior mutability and no shared ownership, so it is `Send`,
/// `Sync`, and cheap to reason about. Posing is a pure function of `q`.
#[derive(Debug)]
pub struct Chain<const N: usize> {
    tree: Tree,
    /// Joint indices from the URDF root down to the tip, in order.
    path: Vec<usize>,
    /// Where each actuated joint sits in `path`.
    actuated: [usize; N],
    /// The inverse of `actuated`: for each slot along `path`, which entry of `q`
    /// drives it, if any. Kept explicitly because `JointSelection::Named` may put
    /// the actuated joints in any order, so their slots are not sorted.
    driven_by: Vec<Option<usize>>,
    /// The entries of `q`, ordered proximal to distal along the chain. The order
    /// of `q` is the caller's (a wire order, under `JointSelection::Named`) and
    /// need not be the linkage's, so anything that walks the linkage - the
    /// dynamics recursions, "which joints move this segment" - walks this.
    proximal_order: [usize; N],
    /// The inverse of `proximal_order`: how far down the chain each entry of `q`
    /// sits, counting from 0 at the base.
    rank_of: [usize; N],
    tip_link: usize,
    base_from_world: Isometry3<f64>,
    axes_local: [Vector3<f64>; N],
    /// How each actuated joint moves, so the Jacobian gives a prismatic joint
    /// its own column rather than assuming everything rotates.
    kinds: [JointKind; N],
    masses: [f64; N],
    coms_local: [Vector3<f64>; N],
    inertias_local: [Matrix3<f64>; N],
    limits: [Limit; N],
    tool: Isometry3<f64>,
}

impl<const N: usize> Chain<N> {
    /// Build from a parsed URDF. `N` is the number of actuated joints the caller
    /// expects; a chain with a different count is an error here, once at load,
    /// which is what lets every type below be fixed-size.
    pub fn from_urdf(robot: &urdf_rs::Robot, spec: &ChainSpec<'_>) -> Result<Self, ChainError> {
        Self::from_tree(Tree::from_robot(robot)?, spec)
    }

    /// Build from an already-walked [`Tree`], for a caller that reads the URDF
    /// once and takes several chains off it (both arms of one robot).
    pub fn from_tree(tree: Tree, spec: &ChainSpec<'_>) -> Result<Self, ChainError> {
        let tip_link = tree
            .link_index(spec.tip_link)
            .ok_or_else(|| ChainError::NoSuchLink(spec.tip_link.to_string()))?;
        let base_link = match spec.base_link {
            None => tree.root(),
            Some(name) => tree
                .link_index(name)
                .ok_or_else(|| ChainError::NoSuchLink(name.to_string()))?,
        };
        let path = tree.path_to(tip_link);
        let base_path = tree.path_to(base_link);
        if !path.starts_with(&base_path) {
            return Err(ChainError::TipNotBelowBase {
                tip: spec.tip_link.to_string(),
                base: tree.link(base_link).name.clone(),
            });
        }

        let actuated = select_joints(&tree, &path, spec.joints)?;
        let axes_local = std::array::from_fn(|i| match tree.joint(path[actuated[i]]).kind {
            JointKind::Revolute { axis } | JointKind::Prismatic { axis } => axis.into_inner(),
            JointKind::Fixed => unreachable!("select_joints only returns movable joints"),
        });
        let limits = actuated_limits::<N>(&tree, &path, &actuated)?;
        refuse_mimic_coupling(&tree, &path, &actuated)?;
        let kinds = std::array::from_fn(|i| tree.joint(path[actuated[i]]).kind);
        let seg = |i: usize| tree.link(tree.joint(path[actuated[i]]).child);
        let mut masses: [f64; N] = std::array::from_fn(|i| seg(i).mass);
        let mut coms_local: [Vector3<f64>; N] = std::array::from_fn(|i| seg(i).com);
        let mut inertias_local: [Matrix3<f64>; N] = std::array::from_fn(|i| seg(i).inertia);

        let mut proximal_order: [usize; N] = std::array::from_fn(|i| i);
        proximal_order.sort_by_key(|&i| actuated[i]);
        let mut rank_of = [0usize; N];
        for (rank, &i) in proximal_order.iter().enumerate() {
            rank_of[i] = rank;
        }

        // A segment's link is not everything that segment carries: a gripper past
        // the tip, a jaw hanging beside it, a bracket bolted to the forearm all
        // move with the last actuated joint above them and weigh on it. Each rides
        // on that joint, and folding it into that segment's inertial here is what
        // lets a dynamics layer read one inertial per segment and be right.
        let mut drives = vec![None; tree.joints().len()];
        for (i, &slot) in actuated.iter().enumerate() {
            drives[path[slot]] = Some(i);
        }
        let rides_on = segments_carrying(&tree, base_link, &drives);
        for i in 0..N {
            let link = tree.joint(path[actuated[i]]).child;
            let carried = Payload::carried_by(&tree, link, i, &rides_on);
            if carried.mass == 0.0 {
                continue;
            }
            let merged = carried.combined_with(masses[i], coms_local[i], inertias_local[i]);
            masses[i] = merged.mass;
            coms_local[i] = merged.com;
            inertias_local[i] = merged.inertia;
        }

        let mut driven_by = vec![None; path.len()];
        for (i, &slot) in actuated.iter().enumerate() {
            driven_by[slot] = Some(i);
        }

        let mut chain = Self {
            tree,
            path,
            actuated,
            driven_by,
            proximal_order,
            rank_of,
            tip_link,
            base_from_world: Isometry3::identity(),
            axes_local,
            kinds,
            masses,
            coms_local,
            inertias_local,
            limits,
            tool: Isometry3::identity(),
        };
        // Every joint above the base is fixed by the `starts_with` check, so the
        // base's world transform at home is its world transform always.
        chain.base_from_world = chain.world_at_home(base_link).inverse();
        Ok(chain)
    }

    /// World transform of a link on the path, at the home configuration.
    fn world_at_home(&self, link: usize) -> Isometry3<f64> {
        let mut here = Isometry3::identity();
        for &j in &self.path {
            let joint = self.tree.joint(j);
            here *= joint.origin * joint.kind.motion(0.0);
            if joint.child == link {
                return here;
            }
        }
        Isometry3::identity()
    }

    /// Raise the reported lower bound of joint `i` to at least `floor`, leaving
    /// the parsed URDF untouched: a control margin the mechanism does not have
    /// (holding a joint off a solver singularity), surfaced through
    /// [`limits`](Self::limits) so every consumer inherits it.
    pub fn with_lower_floor(mut self, i: usize, floor: f64) -> Self {
        self.set_lower_floor(i, floor);
        self
    }

    /// [`with_lower_floor`](Self::with_lower_floor) on an already-built chain.
    pub fn set_lower_floor(&mut self, i: usize, floor: f64) {
        assert!(i < N, "joint index {i} out of range (N = {N})");
        self.limits[i].lo = self.limits[i].lo.max(floor).min(self.limits[i].hi);
    }

    /// Mount a tool frame the URDF carries, which must sit below the tip on
    /// fixed joints only. The chain then reports and is commanded at that frame
    /// throughout, so a caller cannot drive one frame and read another.
    ///
    /// Taking the frame from the URDF rather than from a caller's numbers keeps
    /// the tool where the rest of the robot's geometry lives and makes it a rigid
    /// transform by construction.
    pub fn with_tool_link(mut self, link_name: &str) -> Result<Self, ChainError> {
        self.set_tool_link(link_name)?;
        Ok(self)
    }

    /// [`with_tool_link`](Self::with_tool_link) on an already-built chain.
    pub fn set_tool_link(&mut self, link_name: &str) -> Result<(), ChainError> {
        self.tool = self
            .tree
            .fixed_path_to(self.tip_link, link_name)
            .ok_or_else(|| ChainError::ToolNotFixedBelowTip {
                tool: link_name.to_string(),
                tip: self.tree.link(self.tip_link).name.clone(),
            })?;
        Ok(())
    }

    pub fn limits(&self) -> [Limit; N] {
        self.limits
    }

    /// The mounted `tip -> tool` transform, identity when none is mounted.
    pub fn tool(&self) -> Isometry3<f64> {
        self.tool
    }

    /// The fixed `world -> base` transform resolved from the URDF. Identity when
    /// the base is the URDF root.
    pub fn base_from_world(&self) -> Isometry3<f64> {
        self.base_from_world
    }

    /// Convert a world-frame pose into the base frame this chain reports in.
    pub fn base_pose(&self, world: &Isometry3<f64>) -> Isometry3<f64> {
        self.base_from_world * world
    }

    /// Convert a base-frame pose back into the world frame.
    pub fn world_pose(&self, base: &Isometry3<f64>) -> Isometry3<f64> {
        self.base_from_world.inverse() * base
    }

    /// The underlying URDF tree, for a caller that needs geometry this type does
    /// not surface (collision meshes, links off the chain).
    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// Pose the chain at `q`. Allocation-free: only the actuated joints' frames
    /// and the tip's are retained, which is every frame the readers below need.
    pub fn at(&self, q: &[f64; N]) -> Posed<'_, N> {
        let mut joint_world = [Isometry3::identity(); N];
        let mut here = Isometry3::identity();
        for (slot, &j) in self.path.iter().enumerate() {
            let joint = self.tree.joint(j);
            // Fixed mounts, and movable joints the caller did not name, are frozen
            // and contribute a constant offset.
            let driven = self.driven_by[slot];
            let value = driven.map_or(0.0, |i| q[i]);
            here *= joint.origin * joint.kind.motion(value);
            if let Some(i) = driven {
                joint_world[i] = here;
            }
        }
        Posed {
            chain: self,
            joint_world,
            tip_world: here,
        }
    }
}

/// A chain posed at one configuration: a read-only view, so every pose-dependent
/// quantity can only be read after a pose.
#[derive(Debug)]
pub struct Posed<'a, const N: usize> {
    chain: &'a Chain<N>,
    joint_world: [Isometry3<f64>; N],
    tip_world: Isometry3<f64>,
}

impl<const N: usize> Posed<'_, N> {
    /// End-effector pose in the base frame: the mounted tool's control point, or
    /// the tip when none is mounted.
    pub fn ee_pose(&self) -> Isometry3<f64> {
        self.tip_pose() * self.chain.tool
    }

    /// Tip pose in the base frame, before any mounted tool.
    pub fn tip_pose(&self) -> Isometry3<f64> {
        self.to_base(self.tip_world)
    }

    /// World-frame (URDF root) pose of segment `i`'s link: the link moved by
    /// joint `i`. Two chains of one robot share this frame, so their poses
    /// compose directly.
    pub fn link_pose_world(&self, i: usize) -> Isometry3<f64> {
        self.joint_world[i]
    }

    /// URDF name of segment `i`'s link. Keys per-link data such as collision
    /// geometry.
    pub fn link_name(&self, i: usize) -> &str {
        let joint = self
            .chain
            .tree
            .joint(self.chain.path[self.chain.actuated[i]]);
        &self.chain.tree.link(joint.child).name
    }

    /// Joint `i`'s axis in the base frame. A joint's axis is invariant under its
    /// own value, so rotating the local axis by the joint's world rotation is
    /// exact.
    pub fn axis_base(&self, i: usize) -> Vector3<f64> {
        self.to_base(self.joint_world[i])
            .rotation
            .transform_vector(&self.chain.axes_local[i])
    }

    /// A point on joint `i`'s axis, in the base frame.
    pub fn origin_base(&self, i: usize) -> Vector3<f64> {
        self.to_base(self.joint_world[i]).translation.vector
    }

    /// Joint `i`'s axis in the world frame.
    pub fn axis_world(&self, i: usize) -> Vector3<f64> {
        self.joint_world[i]
            .rotation
            .transform_vector(&self.chain.axes_local[i])
    }

    /// A point on joint `i`'s axis, in the world frame.
    pub fn origin_world(&self, i: usize) -> Vector3<f64> {
        self.joint_world[i].translation.vector
    }

    /// How joint `i` moves. What a dynamics layer dispatches on: a revolute
    /// joint's rate is an angular velocity about its axis, a prismatic one's a
    /// linear velocity along it.
    pub fn kind(&self, i: usize) -> JointKind {
        self.chain.kinds[i]
    }

    /// Mass of segment `i`.
    pub fn mass(&self, i: usize) -> f64 {
        self.chain.masses[i]
    }

    /// World-frame centre of mass of segment `i`.
    pub fn com_world(&self, i: usize) -> Vector3<f64> {
        self.joint_world[i]
            .transform_point(&Point3::from(self.chain.coms_local[i]))
            .coords
    }

    /// World-frame inertia tensor of segment `i` about its centre of mass.
    pub fn inertia_world(&self, i: usize) -> Matrix3<f64> {
        let r = *self.joint_world[i].rotation.to_rotation_matrix().matrix();
        r * self.chain.inertias_local[i] * r.transpose()
    }

    /// The entries of `q` at or below joint `i` on the chain: the segments joint
    /// `i` carries, and the joints that move them. Not `i..N`, because the order
    /// of `q` is the caller's and need not run proximal to distal.
    pub(crate) fn distal_from(&self, i: usize) -> impl Iterator<Item = usize> + '_ {
        self.chain.proximal_order[self.chain.rank_of[i]..]
            .iter()
            .copied()
    }

    /// The entries of `q`, ordered proximal to distal along the chain.
    pub(crate) fn proximal_order(&self) -> [usize; N] {
        self.chain.proximal_order
    }

    /// Geometric Jacobian of the end effector in the base frame. Column `i` is
    /// joint `i`'s contribution: for a revolute joint the linear part is
    /// `zᵢ × (p_ee − pᵢ)` and the angular part is the axis; for a prismatic joint
    /// the linear part is the axis and it contributes no rotation.
    pub fn jacobian(&self) -> Jacobian<N> {
        let p_ee = self.ee_pose().translation.vector;
        let cols: [Vector6<f64>; N] = std::array::from_fn(|i| {
            let z = self.axis_base(i);
            match self.chain.kinds[i] {
                JointKind::Revolute { .. } => {
                    let linear = z.cross(&(p_ee - self.origin_base(i)));
                    Vector6::new(linear.x, linear.y, linear.z, z.x, z.y, z.z)
                }
                JointKind::Prismatic { .. } => Vector6::new(z.x, z.y, z.z, 0.0, 0.0, 0.0),
                JointKind::Fixed => Vector6::zeros(),
            }
        });
        Jacobian::from_columns(&cols)
    }

    /// Linear-velocity Jacobian of a point rigidly attached to `segment`, as
    /// per-joint world-frame contributions: entry `j` is the point's world linear
    /// velocity per unit rate of joint `j`, and is zero for joints distal to the
    /// segment, which do not move it. `point` is in the world frame that
    /// [`link_pose_world`](Self::link_pose_world) returns; a `segment` past the
    /// last joint clamps to the whole chain.
    ///
    /// This is [`jacobian`](Self::jacobian)'s linear rows generalised to an
    /// arbitrary witness point, which is what a collision-distance gradient needs.
    pub fn point_world_jacobian(&self, point: &Point3<f64>, segment: usize) -> [Vector3<f64>; N] {
        let rank_of = &self.chain.rank_of;
        let last_rank = rank_of
            .get(segment)
            .copied()
            .unwrap_or_else(|| N.saturating_sub(1));
        std::array::from_fn(|j| {
            if rank_of[j] > last_rank {
                return Vector3::zeros();
            }
            let z = self.axis_world(j);
            match self.chain.kinds[j] {
                JointKind::Revolute { .. } => z.cross(&(point.coords - self.origin_world(j))),
                JointKind::Prismatic { .. } => z,
                JointKind::Fixed => Vector3::zeros(),
            }
        })
    }

    /// The chain's constant `world -> base` transform, so a caller holding a
    /// posed view does not have to keep the chain to hand as well.
    pub fn base_from_world(&self) -> Isometry3<f64> {
        self.chain.base_from_world
    }

    fn to_base(&self, world: Isometry3<f64>) -> Isometry3<f64> {
        self.chain.base_from_world * world
    }
}

/// The actuated joints' position limits, or a refusal.
///
/// An absent or unbounded range is refused rather than defaulted. A limit that
/// is not a real bound cannot be clamped into, seeded from, or checked against,
/// and inventing one (the conventional +/-pi) silently confines a joint the
/// mechanism turns freely: the chain would then refuse motion the robot can do,
/// which is worse than refusing to load it.
fn actuated_limits<const N: usize>(
    tree: &Tree,
    path: &[usize],
    actuated: &[usize; N],
) -> Result<[Limit; N], ChainError> {
    let mut limits = [Limit { lo: 0.0, hi: 0.0 }; N];
    for (i, limit) in limits.iter_mut().enumerate() {
        let joint = tree.joint(path[actuated[i]]);
        let unusable = |lo: f64, hi: f64| ChainError::UnusableLimit {
            joint: joint.name.clone(),
            lo,
            hi,
        };
        let (lo, hi) = joint.limit.ok_or_else(|| unusable(0.0, 0.0))?;
        if !(lo.is_finite() && hi.is_finite() && lo < hi) {
            return Err(unusable(lo, hi));
        }
        *limit = Limit { lo, hi };
    }
    Ok(limits)
}

/// Refuse a chain whose motion is coupled to a joint it does not drive.
///
/// A URDF `<mimic>` binds one joint's position to another's. Forward kinematics
/// here poses only the joints `q` names, so a coupling with one end on the chain
/// would leave the other end behind and report a frame the mechanism cannot
/// hold. A coupling touching neither end - a gripper's two fingers, distal to the
/// tip - is another mechanism's business and is left alone.
fn refuse_mimic_coupling<const N: usize>(
    tree: &Tree,
    path: &[usize],
    actuated: &[usize; N],
) -> Result<(), ChainError> {
    let driven: Vec<&str> = actuated
        .iter()
        .map(|&slot| tree.joint(path[slot]).name.as_str())
        .collect();
    let coupled = tree.joints().iter().find_map(|j| {
        let leader = j.mimic.as_deref()?;
        (driven.contains(&j.name.as_str()) || driven.contains(&leader)).then(|| {
            ChainError::MimicCoupling {
                follower: j.name.clone(),
                leader: leader.to_string(),
            }
        })
    });
    coupled.map_or(Ok(()), Err)
}

/// Resolve the actuated joints to their positions along `path`.
fn select_joints<const N: usize>(
    tree: &Tree,
    path: &[usize],
    selection: JointSelection<'_>,
) -> Result<[usize; N], ChainError> {
    let slots: Vec<usize> = match selection {
        JointSelection::PathOrder => path
            .iter()
            .enumerate()
            .filter(|(_, j)| tree.joint(**j).kind.is_movable())
            .map(|(slot, _)| slot)
            .collect(),
        JointSelection::Named(names) => names
            .iter()
            .map(|name| {
                if names.iter().filter(|n| *n == name).count() > 1 {
                    return Err(ChainError::DuplicateJoint((*name).to_string()));
                }
                path.iter()
                    .position(|&j| tree.joint(j).name == *name)
                    .ok_or_else(|| ChainError::JointNotOnPath((*name).to_string()))
                    .and_then(|slot| {
                        tree.joint(path[slot])
                            .kind
                            .is_movable()
                            .then_some(slot)
                            .ok_or_else(|| ChainError::JointDoesNotMove((*name).to_string()))
                    })
            })
            .collect::<Result<_, _>>()?,
    };
    let found = slots.len();
    slots
        .try_into()
        .map_err(|_| ChainError::JointCount { expected: N, found })
}
