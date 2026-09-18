//! The runtime model: both arms' convex-hull pieces placed by forward
//! kinematics and the minimum signed distance over the checked pairs.
//!
//! Built once from the URDF plus its collision meshes, queried every tick with
//! the two joint configurations. A query is FK plus GJK over the checked
//! piece-pairs; where pieces overlap, EPA recovers the penetration depth, so
//! the distance is signed and continuous through contact.
//!
//! The checked pairs are derived at construction from the URDF: every body
//! pair except those that cannot inform (two fixed bodies never change
//! distance) or that touch by construction (URDF-adjacent, joint-yoked bodies).
//! The hulls are tight: the reported distance is the true surface clearance, with
//! no safety margin baked into the geometry. Keeping the arms apart is the
//! caller's job (a deployment band over the reported distance), not a margin the
//! model papers over.

use std::collections::HashMap;

use srs_model::nalgebra::{Isometry3, Point3, Unit, Vector3};
use srs_model::{ARM_DOF, Arm, JointVec};

use crate::assemble::fit_bodies;
use crate::clip::ClipRegion;
use crate::gjk::{self, Hull, Placed};
use crate::pairs::PairSpec;
use crate::urdf_collision::{JointKind, ParentJoint, UrdfCollisions, ZERO_AXIS_EPS, place_1dof};
use crate::{BuildError, CollisionError};

/// How a body's hulls reach the world frame.
#[derive(Clone, Copy)]
enum Placement {
    /// Already in world frame (torso, mounts); identity for the whole run.
    Fixed,
    /// Link `i` of the left or right arm; placed by FK every query.
    Left(usize),
    Right(usize),
}

/// A body's bounding sphere in its own local frame: centre over the hull
/// vertices, radius covering the rounded pieces. The radius is rotation
/// invariant, so placing the centre gives a cheap distance lower bound for the
/// broadphase.
struct BoundingSphere {
    center: Point3<f64>,
    radius: f64,
}

impl BoundingSphere {
    fn of(hulls: &[Hull]) -> BoundingSphere {
        let (center, radius) =
            gjk::enclosing_sphere(hulls.iter().map(|h| (h.vertices(), h.inflation())));
        BoundingSphere { center, radius }
    }
}

struct Body {
    name: String,
    /// Convex-hull pieces, in the body's local frame (world for `Fixed`).
    local: Vec<Hull>,
    placement: Placement,
    bound: BoundingSphere,
    /// For a gripper finger: how its hull hangs off its host chain link, so it
    /// can be placed at the live opening. `None` for every ordinary body (whose
    /// hulls sit directly in the link frame).
    finger: Option<Finger>,
}

impl Body {
    /// World pose of this body given its host chain link's world pose and its
    /// side's gripper opening: the link pose directly for an ordinary body, or
    /// composed with the finger offset at `opening` for a gripper finger.
    fn place_on(&self, link_world: Isometry3<f64>, opening: f64) -> Isometry3<f64> {
        match &self.finger {
            Some(f) => link_world * f.offset(opening),
            None => link_world,
        }
    }
}

/// A gripper finger's parent joint, parsed at build into an infallible placer:
/// the joint origin, its unit axis, whether it rotates (revolute) or slides
/// (prismatic), and the finger-joint travel oriented closed-to-open by the
/// build's mesh-separation check (see `assemble::orient_finger_pair`).
/// Validated once here (bounded 1-DOF, non-zero axis) so placing it at any
/// opening cannot fail in the per-tick hot path.
#[derive(Clone, Copy)]
struct Finger {
    origin: Isometry3<f64>,
    axis: Unit<Vector3<f64>>,
    revolute: bool,
    /// Finger-joint position (URDF joint units) at fully closed and fully open.
    closed: f64,
    open: f64,
}

impl Finger {
    /// Parse a finger's parent joint into a validated placer, the single gate
    /// for finger-joint kinds: only a prismatic or revolute finger can be
    /// placed, and its axis must normalize. `closed`/`open` are the
    /// mesh-oriented travel extremes from the fit.
    fn from_joint(
        name: &str,
        joint: &ParentJoint,
        closed: f64,
        open: f64,
    ) -> Result<Self, BuildError> {
        let revolute = match joint.kind {
            JointKind::Revolute => true,
            JointKind::Prismatic => false,
            _ => {
                return Err(BuildError::Geometry(format!(
                    "finger '{name}' joint kind {:?} is not a placeable 1-DOF finger",
                    joint.kind
                )));
            }
        };
        if joint.axis.norm() < ZERO_AXIS_EPS {
            return Err(BuildError::Geometry(format!(
                "finger '{name}' parent joint has a zero axis"
            )));
        }
        Ok(Finger {
            origin: joint.origin,
            axis: Unit::new_normalize(joint.axis),
            revolute,
            closed,
            open,
        })
    }

    /// Placement of the finger hull in its host link's frame at opening `fraction`
    /// in `[0, 1]` (0 = fully closed, 1 = fully open), linearly interpolating the
    /// finger-joint travel. The whole opening pipeline is fraction-native, so the
    /// fraction maps straight onto the joint's own travel for the prismatic (v1)
    /// and revolute (v2) grippers alike.
    fn offset(&self, fraction: f64) -> Isometry3<f64> {
        let q = self.closed + fraction.clamp(0.0, 1.0) * (self.open - self.closed);
        place_1dof(&self.origin, &self.axis, self.revolute, q)
    }

    /// World velocity of a point riding this finger per unit opening fraction,
    /// given the host link's world pose. The joint frame's world rotation and
    /// origin are invariant under the finger's own motion (a joint moves about
    /// its own axis), so the field needs no current fraction: a slide moves every
    /// point along the world axis; a rotation swings the point about the joint
    /// origin. Both scale by the joint travel per unit fraction.
    fn point_velocity_per_fraction(
        &self,
        link_world: &Isometry3<f64>,
        point: &Point3<f64>,
    ) -> Vector3<f64> {
        let joint_world = link_world * self.origin;
        let axis_world = joint_world.rotation * self.axis.into_inner();
        let travel = self.open - self.closed;
        if self.revolute {
            (axis_world * travel).cross(&(point.coords - joint_world.translation.vector))
        } else {
            axis_world * travel
        }
    }
}

/// The parts of a [`Body`] the gradient needs, copied out so the arm's FK can
/// be borrowed mutably while they are in hand: which chain the body rides and,
/// for a gripper finger, its live placer.
#[derive(Clone, Copy)]
struct BodyKinematics {
    placement: Placement,
    finger: Option<Finger>,
}

/// One checked pair, resolved to body indices.
struct Pair {
    a: usize,
    b: usize,
}

/// Best candidate while scanning pairs in [`BimanualCollisionModel::min_distance`].
struct Closest {
    distance: f64,
    a: usize,
    b: usize,
    on_a: Point3<f64>,
    on_b: Point3<f64>,
    /// Separating direction for body `a`; `None` on degenerate core contact.
    normal: Option<Unit<Vector3<f64>>>,
}

/// The closest approach over all checked pairs at one configuration. `distance`
/// is the signed surface distance of the winning pair (negative is penetration,
/// from EPA). The witnesses are the closest points on the two hull surfaces in
/// world frame.
#[derive(Debug, Clone)]
pub struct Proximity<'a> {
    pub distance: f64,
    pub link_a: &'a str,
    pub link_b: &'a str,
    pub on_a: Point3<f64>,
    pub on_b: Point3<f64>,
}

/// The nearest-pair [`Proximity`] at one configuration plus the gradient of its
/// surface distance with respect to each arm's joints and each gripper's opening
/// fraction. `grad_left[j]` is `d(distance)/d(q_left[j])`; `grad_openings[s]` is
/// `d(distance)/d(opening_s)` for side `s` (0 = left, 1 = right), nonzero only
/// when a finger body carries a witness; separating motion has a positive
/// gradient. Computed analytically from the nearest pair's witness points (the
/// gradient of the active pair, by the envelope theorem), so it costs one
/// distance query plus two point Jacobians.
#[derive(Debug, Clone)]
pub struct DistanceGradient<'a> {
    pub proximity: Proximity<'a>,
    pub grad_left: JointVec,
    pub grad_right: JointVec,
    pub grad_openings: [f64; 2],
}

/// One hull piece placed in the world: the vertices, the face triangles
/// indexing them, and any `radius` swept around that core. Enough to draw the
/// exact collision surface: the offset faces, plus edge and vertex fillets when
/// the radius is nonzero. A circumscribing fit needs no sweep to contain its
/// mesh, so its pieces report zero. Runtime queries never materialize this.
pub struct PlacedPiece {
    pub vertices: Vec<Point3<f64>>,
    pub faces: Vec<[usize; 3]>,
    pub radius: f64,
}

/// Per-body world-frame hull pieces: the shape
/// [`BimanualCollisionModel::world_pieces`] returns.
pub type BodyPieces<'a> = Vec<(&'a str, Vec<PlacedPiece>)>;

pub struct BimanualCollisionModel {
    left: Arm,
    right: Arm,
    bodies: Vec<Body>,
    pairs: Vec<Pair>,
    /// Pairs dropped by [`Builder::exclude`], kept for the caller to report.
    excluded: Vec<(String, String)>,
    /// Per-body world pose, refreshed by [`place`](Self::place). Fixed bodies
    /// keep the identity (their hulls are already in world frame).
    world_iso: Vec<Isometry3<f64>>,
    /// Gripper opening per side (0 = left, 1 = right) as a fraction in `[0, 1]`
    /// (0 = fully closed, 1 = fully open), set by
    /// [`set_gripper_openings`](Self::set_gripper_openings). Finger bodies are
    /// placed at this opening every query. Defaults to fully open, the widest
    /// outboard envelope; it is not a full substitute for the real opening
    /// (closed fingers occupy between-jaws space the open placement vacates),
    /// so a caller governing near closed jaws must feed the measured opening
    /// before trusting the clearance.
    openings: [f64; 2],
    /// Per-side, per-joint Lipschitz levers of the min surface distance (m/rad):
    /// the max over checked pairs of the sum of both bodies' per-joint reach
    /// bounds on that side (a same-side pair moves both witnesses with one
    /// arm's joints; see [`body_reaches`]). Rebuilt whenever the pair list
    /// changes; feeds [`clearance_step_bound`](Self::clearance_step_bound).
    levers: [JointVec; 2],
    /// Per-side Lipschitz levers of the min surface distance per unit opening
    /// fraction (m): the max over checked pairs of the paired finger bodies'
    /// surface speed bounds under their own joint travel. Rebuilt with `levers`;
    /// feeds [`clearance_step_bound`](Self::clearance_step_bound).
    opening_levers: [f64; 2],
}

/// Configures and builds a [`BimanualCollisionModel`]; start from
/// [`BimanualCollisionModel::builder`].
pub struct Builder {
    urdf: String,
    meshes_dir: String,
    left_base: String,
    right_base: String,
    exclude: Vec<PairSpec>,
    supplied: HashMap<String, Vec<ClipRegion>>,
}

impl Builder {
    /// Drop these pairs from checking. The caller asserts they can never collide;
    /// the assertion is trusted, not re-derived, so excluding a pair that can in
    /// fact collide silently removes that protection. The names must resolve to
    /// real bodies. Dropped pairs are reported by
    /// [`excluded_pairs`](BimanualCollisionModel::excluded_pairs).
    pub fn exclude(mut self, pairs: &[PairSpec]) -> Self {
        self.exclude.extend_from_slice(pairs);
        self
    }

    /// Decompose a body into clip regions, replacing its auto-fit single hull.
    /// Each region's slice of the mesh gets the same rounded simplified-hull fit
    /// a link gets, so a concave body (a torso) is bound as tightly as the links
    /// are, piece by piece. The regions must jointly cover the body's mesh,
    /// checked at build; see [`ClipRegion`] for the overlap and bound-placement
    /// rules. Naming a body that does not exist errors at build.
    pub fn regions(mut self, body: &str, regions: Vec<ClipRegion>) -> Self {
        self.supplied.insert(body.to_string(), regions);
        self
    }

    /// Fit the bodies (supplied regions override the auto-fit), derive the checked
    /// pairs from the structural rules, and apply the exclusions.
    pub fn build(self) -> Result<BimanualCollisionModel, CollisionError> {
        let mut model = BimanualCollisionModel::assemble(
            &self.urdf,
            &self.meshes_dir,
            &self.left_base,
            &self.right_base,
            &self.supplied,
        )?;
        // Candidate pairs: everything that can inform. Excluded structurally:
        // two world-fixed bodies (their distance never changes), and pairs within
        // two moving joints of each other, same-side or torso against a chain's
        // first links. Those are joint-yoked: shoulder or wrist cluster members
        // orbit each other through their whole range, so their distance swings
        // with every legitimate motion while real contact between them is blocked
        // by the link in between. Cross-arm pairs are always checked.
        let lineage: Vec<(String, Lineage)> = model
            .bodies
            .iter()
            .map(|b| {
                let lineage = match b.placement {
                    Placement::Left(i) => Lineage::Side(0, i + 1),
                    Placement::Right(i) => Lineage::Side(1, i + 1),
                    Placement::Fixed if b.name == self.left_base => Lineage::Side(0, 0),
                    Placement::Fixed if b.name == self.right_base => Lineage::Side(1, 0),
                    Placement::Fixed => Lineage::Torso,
                };
                (b.name.clone(), lineage)
            })
            .collect();

        let mut specs = Vec::new();
        for (i, (a, la)) in lineage.iter().enumerate() {
            for (b, lb) in &lineage[i + 1..] {
                let keep = match (la, lb) {
                    (Lineage::Torso, Lineage::Torso) => false,
                    (Lineage::Side(_, 0), Lineage::Torso)
                    | (Lineage::Torso, Lineage::Side(_, 0)) => false,
                    (Lineage::Side(sa, 0), Lineage::Side(sb, 0)) if sa != sb => false,
                    (Lineage::Side(sa, da), Lineage::Side(sb, db)) if sa == sb => {
                        da.abs_diff(*db) > 2
                    }
                    (Lineage::Torso, Lineage::Side(_, d))
                    | (Lineage::Side(_, d), Lineage::Torso) => *d > 2,
                    (Lineage::Side(..), Lineage::Side(..)) => true,
                };
                if keep {
                    specs.push(PairSpec::new(a.clone(), b.clone()));
                }
            }
        }
        model.set_pairs(&specs)?;
        model.exclude_named(&self.exclude)?;
        Ok(model)
    }
}

impl BimanualCollisionModel {
    /// Start building a model from a URDF string and its collision mesh
    /// directory, naming the two chain base links. See [`Builder`]. The model is
    /// a pure distance oracle: it reports clearances (and, via
    /// [`distance_gradient`](Self::distance_gradient), their gradient), and the
    /// caller decides how to throttle on them.
    pub fn builder(urdf: &str, meshes_dir: &str, left_base: &str, right_base: &str) -> Builder {
        Builder {
            urdf: urdf.to_string(),
            meshes_dir: meshes_dir.to_string(),
            left_base: left_base.to_string(),
            right_base: right_base.to_string(),
            exclude: Vec::new(),
            supplied: HashMap::new(),
        }
    }

    /// Like [`builder`](Self::builder) but reading the URDF from a file.
    pub fn builder_from_file(
        path: &str,
        meshes_dir: &str,
        left_base: &str,
        right_base: &str,
    ) -> Result<Builder, CollisionError> {
        let urdf = std::fs::read_to_string(path)
            .map_err(|e| BuildError::Geometry(format!("read urdf '{path}': {e}")))?;
        Ok(Self::builder(&urdf, meshes_dir, left_base, right_base))
    }

    /// Build the bodies with an explicit checked-pair list and no structural
    /// derivation, bypassing the safety-relevant pair rules. Test-only: the
    /// public path is [`builder`](Self::builder). An empty list builds the
    /// bodies with no checked pairs.
    #[cfg(test)]
    fn with_pairs(
        urdf: &str,
        meshes_dir: &str,
        left_base: &str,
        right_base: &str,
        pair_specs: &[PairSpec],
    ) -> Result<Self, BuildError> {
        let mut model = Self::assemble(urdf, meshes_dir, left_base, right_base, &HashMap::new())?;
        model.set_pairs(pair_specs)?;
        Ok(model)
    }

    /// Fit every collision body (supplied regions override the auto-fit) and place
    /// them, with no checked pairs set yet.
    fn assemble(
        urdf: &str,
        meshes_dir: &str,
        left_base: &str,
        right_base: &str,
        supplied: &HashMap<String, Vec<ClipRegion>>,
    ) -> Result<Self, BuildError> {
        if left_base == right_base {
            return Err(BuildError::IdenticalBases {
                base: left_base.to_string(),
            });
        }
        let mut left = Arm::from_urdf(urdf, left_base)?;
        let mut right = Arm::from_urdf(urdf, right_base)?;

        let home = [0.0; ARM_DOF];
        let chain_names = |arm: &mut Arm| -> Vec<String> {
            let posed = arm.at(&home);
            (0..ARM_DOF)
                .map(|i| posed.link_name(i).to_string())
                .collect()
        };
        let left_names = chain_names(&mut left);
        let right_names = chain_names(&mut right);

        let parsed = UrdfCollisions::from_urdf(urdf)?;
        let fitted = fit_bodies(
            &parsed,
            &[left_names.clone(), right_names.clone()],
            meshes_dir,
            supplied,
        )?;

        let mut bodies: Vec<Body> = Vec::new();
        let push_body = |bodies: &mut Vec<Body>, body: Body| -> Result<(), BuildError> {
            if bodies.iter().any(|b| b.name == body.name) {
                return Err(BuildError::DuplicateBody {
                    name: body.name.clone(),
                });
            }
            bodies.push(body);
            Ok(())
        };
        let mut links = fitted.links;
        for (name, hulls) in fitted.fixed {
            let bound = BoundingSphere::of(&hulls);
            push_body(
                &mut bodies,
                Body {
                    name,
                    local: hulls,
                    placement: Placement::Fixed,
                    bound,
                    finger: None,
                },
            )?;
        }
        for (names, side_left) in [(&left_names, true), (&right_names, false)] {
            for (i, name) in names.iter().enumerate() {
                let hulls = links
                    .remove(name)
                    .ok_or_else(|| BuildError::SharedLink { name: name.clone() })?;
                let placement = if side_left {
                    Placement::Left(i)
                } else {
                    Placement::Right(i)
                };
                let bound = BoundingSphere::of(&hulls);
                push_body(
                    &mut bodies,
                    Body {
                        name: name.clone(),
                        local: hulls,
                        placement,
                        bound,
                        finger: None,
                    },
                )?;
            }
        }
        // Finger bodies hang off their host chain link's FK segment (so they share
        // its lineage: not checked against their own hand or sibling finger, but
        // checked cross-arm and against the torso) and carry a `Finger` placer so
        // each query positions them at the live opening.
        for finger in fitted.fingers {
            let placement = chain_segment(&left_names, &right_names, &finger.parent_link)
                .expect("fit_bodies only emits fingers hosted on a chain link");
            let placer =
                Finger::from_joint(&finger.name, &finger.joint, finger.closed, finger.open)?;
            let bound = BoundingSphere::of(&finger.hulls);
            push_body(
                &mut bodies,
                Body {
                    name: finger.name,
                    local: finger.hulls,
                    placement,
                    bound,
                    finger: Some(placer),
                },
            )?;
        }

        let world_iso = vec![Isometry3::identity(); bodies.len()];
        Ok(Self {
            left,
            right,
            bodies,
            pairs: Vec::new(),
            excluded: Vec::new(),
            world_iso,
            openings: [1.0, 1.0],
            levers: [[0.0; ARM_DOF]; 2],
            opening_levers: [0.0; 2],
        })
    }

    /// Drop the caller's named exclusions (see [`Builder::exclude`]). The names
    /// must resolve to real bodies, but the assertion that the pair cannot
    /// collide is trusted, not re-derived: a pair that is not currently checked
    /// is a harmless no-op.
    fn exclude_named(&mut self, exclude: &[PairSpec]) -> Result<(), BuildError> {
        for spec in exclude {
            let a = self.body_index(&spec.a)?;
            let b = self.body_index(&spec.b)?;
            let is_pair = |p: &Pair| (p.a == a && p.b == b) || (p.a == b && p.b == a);
            let before = self.pairs.len();
            self.pairs.retain(|p| !is_pair(p));
            if self.pairs.len() < before {
                self.excluded
                    .push((self.bodies[a].name.clone(), self.bodies[b].name.clone()));
            }
        }
        self.recompute_levers();
        Ok(())
    }

    /// The pairs dropped by [`Builder::exclude`], for the caller to report.
    pub fn excluded_pairs(&self) -> &[(String, String)] {
        &self.excluded
    }

    fn body_index(&self, name: &str) -> Result<usize, BuildError> {
        self.bodies
            .iter()
            .position(|b| b.name == name)
            .ok_or_else(|| BuildError::UnknownBody {
                name: name.to_string(),
            })
    }

    /// Replace the checked pair list (names resolved against the bodies).
    fn set_pairs(&mut self, pair_specs: &[PairSpec]) -> Result<(), BuildError> {
        let index: HashMap<&str, usize> = self
            .bodies
            .iter()
            .enumerate()
            .map(|(i, b)| (b.name.as_str(), i))
            .collect();
        self.pairs = pair_specs
            .iter()
            .map(|p| {
                let a = *index
                    .get(p.a.as_str())
                    .ok_or_else(|| BuildError::UnknownPairBody { name: p.a.clone() })?;
                let b = *index
                    .get(p.b.as_str())
                    .ok_or_else(|| BuildError::UnknownPairBody { name: p.b.clone() })?;
                if a == b {
                    return Err(BuildError::SelfPair { name: p.a.clone() });
                }
                Ok(Pair { a, b })
            })
            .collect::<Result<Vec<_>, BuildError>>()?;
        self.recompute_levers();
        Ok(())
    }

    /// Rebuild the per-side Lipschitz levers from the checked pairs: for each
    /// side and joint, the max over pairs of the sum of both bodies' reaches on
    /// that side. Summing per pair is what keeps the bound sound for a same-side
    /// pair, whose two witnesses both move with that arm's joints. Called
    /// whenever the pair list changes (build-time only, so recomputing the
    /// per-body reaches here is free).
    fn recompute_levers(&mut self) {
        let reaches = body_reaches(&mut self.left, &mut self.right, &self.bodies);
        let side_reach = |body: usize, side: usize, j: usize| -> f64 {
            let on_side = match self.bodies[body].placement {
                Placement::Left(_) => side == 0,
                Placement::Right(_) => side == 1,
                Placement::Fixed => false,
            };
            if on_side { reaches[body][j] } else { 0.0 }
        };
        self.levers = std::array::from_fn(|side| {
            std::array::from_fn(|j| {
                self.pairs
                    .iter()
                    .map(|p| side_reach(p.a, side, j) + side_reach(p.b, side, j))
                    .fold(0.0, f64::max)
            })
        });
        // A finger body's surface moves at most `opening_reach` metres per unit
        // opening fraction: the full joint travel, times (revolute only) the
        // farthest hull point's distance from the joint axis (the axis passes
        // through the finger frame origin along `axis`, so the lever arm of a
        // local point is its component perpendicular to the axis) plus the
        // inflation radius. Non-finger bodies do not move with an opening.
        let opening_reach = |body: &Body| -> f64 {
            let Some(f) = &body.finger else { return 0.0 };
            let travel = (f.open - f.closed).abs();
            if !f.revolute {
                return travel;
            }
            let r_max = body
                .local
                .iter()
                .flat_map(|h| {
                    let radius = h.inflation();
                    h.vertices().iter().map(move |p| {
                        let along = p.coords.dot(&f.axis);
                        (p.coords - f.axis.into_inner() * along).norm() + radius
                    })
                })
                .fold(0.0, f64::max);
            travel * r_max
        };
        let side_opening_reach = |body: usize, side: usize| -> f64 {
            let on_side = match self.bodies[body].placement {
                Placement::Left(_) => side == 0,
                Placement::Right(_) => side == 1,
                Placement::Fixed => false,
            };
            if on_side {
                opening_reach(&self.bodies[body])
            } else {
                0.0
            }
        };
        self.opening_levers = std::array::from_fn(|side| {
            self.pairs
                .iter()
                .map(|p| side_opening_reach(p.a, side) + side_opening_reach(p.b, side))
                .fold(0.0, f64::max)
        });
    }

    /// Link-local hull pieces of a body (fixed bodies are in the root frame).
    /// Exposes the internal [`Hull`], so it is test-only.
    #[cfg(test)]
    fn local_hulls(&self, name: &str) -> Option<&[Hull]> {
        self.bodies
            .iter()
            .find(|b| b.name == name)
            .map(|b| b.local.as_slice())
    }

    /// All checked pairs by name, for diagnostics and tests.
    pub fn checked_pairs(&self) -> Vec<(&str, &str)> {
        self.pairs
            .iter()
            .map(|p| {
                (
                    self.bodies[p.a].name.as_str(),
                    self.bodies[p.b].name.as_str(),
                )
            })
            .collect()
    }

    /// The nearest checked pair at the given configurations: places the hulls by
    /// FK, then scans the pairs (broadphase-ordered) for the minimum signed
    /// distance. The shared core of [`min_distance`](Self::min_distance) and
    /// [`distance_gradient`](Self::distance_gradient).
    fn closest(
        &mut self,
        q_left: &JointVec,
        q_right: &JointVec,
    ) -> Result<Closest, CollisionError> {
        self.place(q_left, q_right);

        // Broadphase: a pair's bounding-sphere gap is a lower bound on its true
        // distance. Scanned in ascending order of that bound, once it exceeds
        // the best distance found no remaining pair can win, so the scan stops.
        let centers: Vec<Point3<f64>> = self
            .bodies
            .iter()
            .zip(&self.world_iso)
            .map(|(b, iso)| iso * b.bound.center)
            .collect();
        let mut order: Vec<(f64, usize)> = self
            .pairs
            .iter()
            .enumerate()
            .map(|(i, p)| {
                (
                    (centers[p.a] - centers[p.b]).norm()
                        - self.bodies[p.a].bound.radius
                        - self.bodies[p.b].bound.radius,
                    i,
                )
            })
            .collect();
        order.sort_by(|x, y| x.0.total_cmp(&y.0));

        let mut best: Option<Closest> = None;
        for (lower_bound, i) in order {
            if best.as_ref().is_some_and(|c| lower_bound > c.distance) {
                break;
            }
            let pair = &self.pairs[i];
            let (iso_a, iso_b) = (self.world_iso[pair.a], self.world_iso[pair.b]);
            // The transformed piece centres of body b are constant across body
            // a's pieces; place them once per pair, not once per (ha, hb).
            let centers_b: Vec<Point3<f64>> = self.bodies[pair.b]
                .local
                .iter()
                .map(|hb| iso_b * hb.bound_center())
                .collect();
            for ha in &self.bodies[pair.a].local {
                let center_a = iso_a * ha.bound_center();
                for (hb, center_b) in self.bodies[pair.b].local.iter().zip(&centers_b) {
                    // Piece-level prefilter, same sphere bound as the pair
                    // broadphase: a piece pair that cannot beat the best
                    // distance skips its GJK. A multi-piece body (the torso's
                    // region decomposition) otherwise pays one GJK per piece
                    // for pieces nowhere near the query.
                    let gap = (center_a - center_b).norm() - ha.bound_radius() - hb.bound_radius();
                    if best.as_ref().is_some_and(|c| gap > c.distance) {
                        continue;
                    }
                    let r = gjk::distance(&Placed::new(ha, iso_a), &Placed::new(hb, iso_b));
                    if best.as_ref().is_none_or(|c| r.distance < c.distance) {
                        best = Some(Closest {
                            distance: r.distance,
                            a: pair.a,
                            b: pair.b,
                            on_a: r.on_a,
                            on_b: r.on_b,
                            normal: r.normal,
                        });
                    }
                }
            }
        }
        best.ok_or(CollisionError::NoPairs)
    }

    /// Minimum signed distance over all checked pairs at the given
    /// configurations, with the witness points. Non-finite joint values are
    /// rejected so the caller fails safe rather than comparing against NaN.
    pub fn min_distance(
        &mut self,
        q_left: &JointVec,
        q_right: &JointVec,
    ) -> Result<Proximity<'_>, CollisionError> {
        ensure_finite(q_left, q_right)?;
        let c = self.closest(q_left, q_right)?;
        Ok(Proximity {
            distance: c.distance,
            link_a: &self.bodies[c.a].name,
            link_b: &self.bodies[c.b].name,
            on_a: c.on_a,
            on_b: c.on_b,
        })
    }

    /// The nearest-pair [`Proximity`] and the analytic gradient of its distance
    /// with respect to each arm's joints (see [`DistanceGradient`]). The gradient
    /// is the nearest pair's separating direction projected through each witness
    /// point's velocity Jacobian, so it reflects the same min-over-pairs distance
    /// `min_distance` returns at one distance query's cost. Fails on a non-finite
    /// configuration, or on degenerate core contact (the rounded surfaces then
    /// overlap by the summed radii) where no separating direction is defined; a
    /// velocity-barrier caller holds there.
    pub fn distance_gradient(
        &mut self,
        q_left: &JointVec,
        q_right: &JointVec,
    ) -> Result<DistanceGradient<'_>, CollisionError> {
        ensure_finite(q_left, q_right)?;
        let c = self.closest(q_left, q_right)?;
        // GJK carries the separating direction explicitly (the witness difference
        // reverses sense with the sign of the distance); the +1/-1 signs below
        // sum the projected witness velocities to d(distance)/dq.
        let Some(normal) = c.normal.map(Unit::into_inner) else {
            return Err(CollisionError::WitnessesCoincide {
                distance: c.distance,
            });
        };
        let kinematics = |body: &Body| BodyKinematics {
            placement: body.placement,
            finger: body.finger,
        };
        let (kin_a, kin_b) = (kinematics(&self.bodies[c.a]), kinematics(&self.bodies[c.b]));
        let (left_a, right_a, open_a) =
            self.gradient_contribution(kin_a, &c.on_a, &normal, 1.0, q_left, q_right);
        let (left_b, right_b, open_b) =
            self.gradient_contribution(kin_b, &c.on_b, &normal, -1.0, q_left, q_right);
        Ok(DistanceGradient {
            proximity: Proximity {
                distance: c.distance,
                link_a: &self.bodies[c.a].name,
                link_b: &self.bodies[c.b].name,
                on_a: c.on_a,
                on_b: c.on_b,
            },
            grad_left: std::array::from_fn(|j| left_a[j] + left_b[j]),
            grad_right: std::array::from_fn(|j| right_a[j] + right_b[j]),
            grad_openings: [open_a[0] + open_b[0], open_a[1] + open_b[1]],
        })
    }

    /// One body's contribution to the distance gradient: the pair's separating
    /// direction `normal` projected through the witness `point`'s velocity
    /// Jacobian, plus, for a finger body, through the point's velocity per unit
    /// opening fraction. `sign` is +1 for body a and -1 for body b (`normal`
    /// increases the distance for a, decreases it for b). A world-fixed body
    /// (torso) contributes nothing.
    fn gradient_contribution(
        &mut self,
        kinematics: BodyKinematics,
        point: &Point3<f64>,
        normal: &Vector3<f64>,
        sign: f64,
        q_left: &JointVec,
        q_right: &JointVec,
    ) -> (JointVec, JointVec, [f64; 2]) {
        let BodyKinematics { placement, finger } = kinematics;
        let zero: JointVec = [0.0; ARM_DOF];
        let contribution = |arm: &mut Arm, q: &JointVec, segment: usize| -> (JointVec, f64) {
            let posed = arm.at(q);
            let cols = posed.point_world_jacobian(point, segment);
            let joints = std::array::from_fn(|j| sign * normal.dot(&cols[j]));
            let opening = finger.map_or(0.0, |f| {
                let v = f.point_velocity_per_fraction(&posed.link_pose_world(segment), point);
                sign * normal.dot(&v)
            });
            (joints, opening)
        };
        match placement {
            Placement::Fixed => (zero, zero, [0.0; 2]),
            Placement::Left(s) => {
                let (joints, opening) = contribution(&mut self.left, q_left, s);
                (joints, zero, [opening, 0.0])
            }
            Placement::Right(s) => {
                let (joints, opening) = contribution(&mut self.right, q_right, s);
                (zero, joints, [0.0, opening])
            }
        }
    }

    /// True if any checked pair is at or below `threshold`.
    pub fn in_collision(
        &mut self,
        q_left: &JointVec,
        q_right: &JointVec,
        threshold: f64,
    ) -> Result<bool, CollisionError> {
        if !threshold.is_finite() {
            return Err(CollisionError::NonFinite);
        }
        Ok(self.min_distance(q_left, q_right)?.distance <= threshold)
    }

    /// World-frame hull pieces of every body at the given configuration, paired
    /// with the body name (for visualization; runtime queries use
    /// [`min_distance`](Self::min_distance)). Each piece carries its placed
    /// vertices, the face triangles, and the inflation radius, so a caller can
    /// draw the true rounded collision surface, not just the bare core.
    pub fn world_pieces(
        &mut self,
        q_left: &JointVec,
        q_right: &JointVec,
    ) -> Result<BodyPieces<'_>, CollisionError> {
        ensure_finite(q_left, q_right)?;
        self.place(q_left, q_right);
        Ok(self
            .bodies
            .iter()
            .zip(&self.world_iso)
            .map(|(b, iso)| {
                let pieces = b
                    .local
                    .iter()
                    .map(|h| PlacedPiece {
                        vertices: h.vertices().iter().map(|v| iso * v).collect(),
                        faces: h.faces().to_vec(),
                        radius: h.inflation(),
                    })
                    .collect();
                (b.name.as_str(), pieces)
            })
            .collect())
    }

    /// Set the gripper opening per side as a fraction in `[0, 1]` (0 = fully
    /// closed, 1 = fully open); values are clamped. Finger bodies are placed at
    /// this opening on every subsequent query, so the reported clearance follows
    /// the fingers' true positions instead of their full swept envelope. A
    /// non-finite value is ignored for that side (the last good opening stands),
    /// so a bad reading never poisons the placement.
    pub fn set_gripper_openings(&mut self, left: f64, right: f64) {
        if left.is_finite() {
            self.openings[0] = left.clamp(0.0, 1.0);
        }
        if right.is_finite() {
            self.openings[1] = right.clamp(0.0, 1.0);
        }
    }

    /// Upper bound (m) on how much the minimum surface distance can change over a
    /// step of `dq_left` / `dq_right` on the arm joints and `dopenings` on the
    /// gripper opening fractions, valid along the whole straight segment in that
    /// combined space: `sum_j levers[side][j] * |dq[j]|` over both arms plus
    /// `opening_levers[side] * |dopenings[side]|` over both grippers. Each lever
    /// bounds, over all poses and openings, the worst per-unit closing rate of
    /// any checked pair (summing both witnesses' travel when a pair's bodies
    /// share a side), and a minimum of Lipschitz functions is Lipschitz, so a
    /// segment whose start clearance exceeds a floor by more than this bound
    /// cannot cross that floor anywhere along the step. Deliberately loose
    /// (chain-length bounds), so it is sound for a caller skipping an exact scan,
    /// never tight.
    pub fn clearance_step_bound(
        &self,
        dq_left: &JointVec,
        dq_right: &JointVec,
        dopenings: &[f64; 2],
    ) -> f64 {
        let dot_abs = |lever: &JointVec, dq: &JointVec| -> f64 {
            lever.iter().zip(dq).map(|(l, d)| l * d.abs()).sum::<f64>()
        };
        let bound = dot_abs(&self.levers[0], dq_left)
            + dot_abs(&self.levers[1], dq_right)
            + self.opening_levers[0] * dopenings[0].abs()
            + self.opening_levers[1] * dopenings[1].abs();
        // A non-finite delta gives no finite bound: return infinity so a caller's
        // skip predicate (`margin > bound`) can never pass on bad data, rather
        // than a NaN whose comparison direction the caller must not rely on.
        if bound.is_finite() {
            bound
        } else {
            f64::INFINITY
        }
    }

    /// Refresh the world pose of the moving bodies from FK. Finger bodies are
    /// additionally offset by their host link pose at the side's current opening.
    fn place(&mut self, q_left: &JointVec, q_right: &JointVec) {
        let poses_l = link_poses(&mut self.left, q_left);
        let poses_r = link_poses(&mut self.right, q_right);
        let openings = self.openings;
        for (body, iso) in self.bodies.iter().zip(self.world_iso.iter_mut()) {
            *iso = match body.placement {
                Placement::Fixed => continue,
                Placement::Left(i) => body.place_on(poses_l[i], openings[0]),
                Placement::Right(i) => body.place_on(poses_r[i], openings[1]),
            };
        }
    }
}

/// The [`Placement`] of a body hanging off chain link `parent`: `Left`/`Right`
/// with the link's FK segment index, or `None` if `parent` is not a chain link.
fn chain_segment(left: &[String], right: &[String], parent: &str) -> Option<Placement> {
    if let Some(i) = left.iter().position(|n| n == parent) {
        return Some(Placement::Left(i));
    }
    right.iter().position(|n| n == parent).map(Placement::Right)
}

/// Where a body sits in the kinematic tree, for the structural pair rules:
/// the torso, or chain side plus moving-joint depth (mount = 0, link k = k).
enum Lineage {
    Torso,
    Side(u8, usize),
}

fn link_poses(arm: &mut Arm, q: &JointVec) -> [Isometry3<f64>; ARM_DOF] {
    let posed = arm.at(q);
    std::array::from_fn(|i| posed.link_pose_world(i))
}

/// Per-body, per-joint surface-speed bounds (m/rad). A point rigidly attached
/// distal of revolute joint `j` moves at most `r * |dq_j|`, with `r` its distance
/// from the joint axis; that distance is bounded, over all poses, by the chain
/// hops from joint `j`'s origin out to the body's link origin (each hop's norm is
/// pose invariant: a rigid link separates consecutive joint origins) plus the
/// body's own reach: bounding-sphere centre offset + radius, and for a finger the
/// worst translation its joint offset can add across the travel (a prismatic
/// slide adds the full travel; a revolute offset only rotates about its origin).
/// Rows are zero for fixed bodies and for joints distal of the body.
fn body_reaches(left: &mut Arm, right: &mut Arm, bodies: &[Body]) -> Vec<JointVec> {
    let home = [0.0; ARM_DOF];
    let hops = |arm: &mut Arm| -> [f64; ARM_DOF - 1] {
        let poses = link_poses(arm, &home);
        std::array::from_fn(|k| {
            (poses[k + 1].translation.vector - poses[k].translation.vector).norm()
        })
    };
    let hops = [hops(left), hops(right)];

    bodies
        .iter()
        .map(|body| {
            let (side, seg) = match body.placement {
                Placement::Fixed => return [0.0; ARM_DOF], // never moved by a joint
                Placement::Left(s) => (0, s),
                Placement::Right(s) => (1, s),
            };
            let finger_reach = body.finger.as_ref().map_or(0.0, |f| {
                let travel = if f.revolute {
                    0.0
                } else {
                    f.closed.abs().max(f.open.abs())
                };
                f.origin.translation.vector.norm() + travel
            });
            let reach = finger_reach + body.bound.center.coords.norm() + body.bound.radius;
            std::array::from_fn(|j| {
                if j <= seg {
                    hops[side][j..seg].iter().sum::<f64>() + reach
                } else {
                    0.0 // a joint distal of the body does not move it
                }
            })
        })
        .collect()
}

/// Reject NaN/inf joint values so queries fail safe instead of comparing
/// against NaN downstream.
fn ensure_finite(q_left: &JointVec, q_right: &JointVec) -> Result<(), CollisionError> {
    if q_left.iter().chain(q_right).any(|x| !x.is_finite()) {
        return Err(CollisionError::NonFinite);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairs::PairSpec;

    const URDF: &str = include_str!("../tests/fixtures/openarm_v10.urdf");
    const MESHES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/meshes");

    fn model() -> BimanualCollisionModel {
        BimanualCollisionModel::builder(URDF, MESHES, "openarm_left_link0", "openarm_right_link0")
            .build()
            .expect("model")
    }

    const INF: f64 = f64::INFINITY;

    fn region(min: [f64; 3], max: [f64; 3]) -> ClipRegion {
        ClipRegion::new(
            Point3::new(min[0], min[1], min[2]),
            Point3::new(max[0], max[1], max[2]),
        )
        .expect("test region")
    }

    // Two overlapping z-slabs that jointly cover the torso mesh. Just enough to
    // exercise the multi-piece decomposition path; the tuned deployment regions
    // live in the shared tests/fixtures/openarm.rs, exercised by the
    // integration test.
    fn covering_regions() -> Vec<ClipRegion> {
        vec![
            region([-INF, -INF, -INF], [INF, INF, 0.404]),
            region([-INF, -INF, 0.396], [INF, INF, INF]),
        ]
    }

    fn build(pairs: &[PairSpec]) -> Result<BimanualCollisionModel, BuildError> {
        BimanualCollisionModel::with_pairs(
            URDF,
            MESHES,
            "openarm_left_link0",
            "openarm_right_link0",
            pairs,
        )
    }

    /// Check every analytic gradient column (both arms' joints and both opening
    /// fractions) against central differences at one configuration, with the
    /// grippers at mid-travel 0.6 so the finger bodies participate with a
    /// nontrivial live offset (the opening is a constant across each FD
    /// perturbation, exactly as it is across one governor tick).
    fn assert_gradient_matches_finite_difference(
        m: &mut BimanualCollisionModel,
        ql: &JointVec,
        qr: &JointVec,
    ) {
        let h = 1e-5;
        m.set_gripper_openings(0.6, 0.6);
        let grad = m.distance_gradient(ql, qr).expect("gradient defined");
        let (analytic_left, analytic_right) = (grad.grad_left, grad.grad_right);
        let analytic_openings = grad.grad_openings;
        // Opening columns against central differences on the fractions, the
        // same envelope-theorem check as the joints below.
        for s in 0..2 {
            let openings_at = |frac: f64| -> [f64; 2] {
                let mut o = [0.6, 0.6];
                o[s] = frac;
                o
            };
            let probe = |m: &mut BimanualCollisionModel, frac: f64| -> f64 {
                let o = openings_at(frac);
                m.set_gripper_openings(o[0], o[1]);
                let d = m.min_distance(ql, qr).unwrap().distance;
                m.set_gripper_openings(0.6, 0.6);
                d
            };
            let fd = (probe(m, 0.6 + h) - probe(m, 0.6 - h)) / (2.0 * h);
            assert!(
                (analytic_openings[s] - fd).abs() < 3e-3,
                "opening {s}: analytic {} fd {fd}",
                analytic_openings[s]
            );
        }
        for j in 0..ARM_DOF {
            let mut lp = *ql;
            let mut lm = *ql;
            lp[j] += h;
            lm[j] -= h;
            let fd_left = (m.min_distance(&lp, qr).unwrap().distance
                - m.min_distance(&lm, qr).unwrap().distance)
                / (2.0 * h);
            let mut rp = *qr;
            let mut rm = *qr;
            rp[j] += h;
            rm[j] -= h;
            let fd_right = (m.min_distance(ql, &rp).unwrap().distance
                - m.min_distance(ql, &rm).unwrap().distance)
                / (2.0 * h);
            assert!(
                (analytic_left[j] - fd_left).abs() < 3e-3,
                "left j{j}: analytic {} fd {fd_left}",
                analytic_left[j]
            );
            assert!(
                (analytic_right[j] - fd_right).abs() < 3e-3,
                "right j{j}: analytic {} fd {fd_right}",
                analytic_right[j]
            );
        }
    }

    #[test]
    fn distance_gradient_matches_finite_difference_in_penetration() {
        let mut m = model();
        m.set_gripper_openings(0.6, 0.6);
        // Wrists folded inward but ASYMMETRICALLY, so one moving cross-arm pair is
        // unambiguously nearest (a symmetric pose sits on a pair-switch tie where
        // the analytic gradient and a straddling central difference legitimately
        // disagree). Every config penetrates this model's auto-fit torso hull
        // (asserted below), covering deep EPA; the companion test below walks
        // through contact. The last config has a finger body nearest.
        let configs: [(JointVec, JointVec); 4] = [
            (
                [0.15, 0.1, 0.85, 0.5, -0.2, 0.1, 0.0],
                [-0.05, -0.25, -0.45, 0.35, 0.1, -0.1, 0.0],
            ),
            (
                [0.0, 0.3, 0.95, 0.45, 0.1, 0.0, 0.0],
                [0.0, -0.1, -0.55, 0.4, 0.0, 0.1, 0.0],
            ),
            (
                [0.25, -0.1, 0.6, 0.65, 0.0, 0.2, 0.1],
                [-0.1, 0.05, -0.7, 0.3, 0.0, -0.2, 0.0],
            ),
            (
                [0.0, 0.0, 0.95, 0.4, 0.1, 0.0, 0.2],
                [0.0, 0.0, -1.05, 0.4, -0.1, 0.1, 0.0],
            ),
        ];
        {
            let (ql, qr) = &configs[3];
            let p = m.min_distance(ql, qr).expect("query");
            assert!(
                p.link_a.contains("finger") || p.link_b.contains("finger"),
                "setup: the finger config's nearest pair should involve a finger, got {} vs {}",
                p.link_a,
                p.link_b
            );
        }
        for (ql, qr) in configs {
            let d = m.min_distance(&ql, &qr).expect("query").distance;
            assert!(d < 0.0, "setup: expected a penetrating config, got d={d}");
            assert_gradient_matches_finite_difference(&mut m, &ql, &qr);
        }
    }

    #[test]
    fn distance_gradient_matches_finite_difference_across_contact() {
        // The default model's auto-fit torso hull swallows every reachable pose,
        // so the test above only ever sees deep penetration. Restrict the pairs
        // to cross-arm wrist and finger bodies and walk an asymmetric
        // wrists-inward family from clearance through contact into shallow
        // overlap, the regimes where the witness separation reverses sense.
        let mut cross_pairs = Vec::new();
        for a in ["link6", "link7", "left_finger", "right_finger"] {
            for b in ["link6", "link7", "left_finger", "right_finger"] {
                cross_pairs.push(PairSpec::new(
                    format!("openarm_left_{a}"),
                    format!("openarm_right_{b}"),
                ));
            }
        }
        let mut m = BimanualCollisionModel::with_pairs(
            URDF,
            MESHES,
            "openarm_left_link0",
            "openarm_right_link0",
            &cross_pairs,
        )
        .expect("cross-arm model");
        m.set_gripper_openings(0.6, 0.6);
        let pose_at = |t: f64| -> (JointVec, JointVec) {
            (
                [0.1, 0.05, t, 0.45, -0.1, 0.05, 0.0],
                [-0.05, -0.1, -t - 0.08, 0.4, 0.1, -0.05, 0.0],
            )
        };
        let (mut separated, mut penetrating) = (0, 0);
        let mut finger_pair_covered = false;
        for i in 0..=60 {
            let (ql, qr) = pose_at(i as f64 * 0.02);
            let p = m.min_distance(&ql, &qr).expect("query");
            // Stop before the nearest pair switches bodies (a pair-switch tie
            // would make the central difference straddle two gradients).
            if p.distance <= -0.03 {
                break;
            }
            if p.distance > 0.0 {
                separated += 1;
            } else {
                penetrating += 1;
            }
            finger_pair_covered |= p.link_a.contains("finger") || p.link_b.contains("finger");
            assert_gradient_matches_finite_difference(&mut m, &ql, &qr);
        }
        assert!(
            separated >= 3 && penetrating >= 3 && finger_pair_covered,
            "setup: expected configs on both sides of contact with a finger pair nearest, \
             got {separated} separated / {penetrating} penetrating (finger: {finger_pair_covered})"
        );
    }

    #[test]
    fn rejects_unknown_pairs_and_querying_with_no_pairs() {
        assert!(matches!(
            build(&[PairSpec::new("openarm_left_link1", "no_such_body")]).err(),
            Some(BuildError::UnknownPairBody { .. })
        ));
        let mut empty = build(&[]).expect("bodies build without pairs");
        assert!(
            empty
                .min_distance(&[0.0; ARM_DOF], &[0.0; ARM_DOF])
                .is_err()
        );
    }

    #[test]
    fn rejects_self_pairs_and_identical_bases() {
        assert!(build(&[PairSpec::new("openarm_left_link7", "openarm_left_link7")]).is_err());
        let e = BimanualCollisionModel::builder(
            URDF,
            MESHES,
            "openarm_left_link0",
            "openarm_left_link0",
        )
        .build()
        .err()
        .expect("identical bases must fail");
        assert!(
            matches!(&e, CollisionError::Build(BuildError::IdenticalBases { .. })),
            "{e}"
        );
    }

    #[test]
    fn derived_pairs_skip_fixed_pairs_and_adjacency() {
        let m = model();
        let checked: Vec<(String, String)> = m
            .checked_pairs()
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        let has = |a: &str, b: &str| {
            checked
                .iter()
                .any(|(x, y)| (x == a && y == b) || (x == b && y == a))
        };
        // Two fixed bodies never change distance; same-side within two joints
        // is joint-yoked noise.
        assert!(!has("openarm_left_link0", "openarm_right_link0"));
        assert!(!has("openarm_body_link0", "openarm_left_link0"));
        assert!(!has("openarm_left_link0", "openarm_left_link1"));
        assert!(!has("openarm_left_link3", "openarm_left_link4"));
        assert!(!has("openarm_body_link0", "openarm_left_link2"));
        // Beyond the horizon, and cross-arm, are checked.
        assert!(has("openarm_left_link1", "openarm_left_link7"));
        assert!(has("openarm_left_link0", "openarm_left_link4"));
        assert!(has("openarm_body_link0", "openarm_left_link3"));
        assert!(has("openarm_left_link7", "openarm_right_link7"));
    }

    #[test]
    fn auto_fit_is_one_hull_per_body() {
        let m = model();
        assert_eq!(
            m.local_hulls("openarm_body_link0").expect("torso").len(),
            1,
            "auto-fit is a single hull"
        );
        assert_eq!(
            m.local_hulls("openarm_left_link7").expect("gripper").len(),
            1
        );
    }

    #[test]
    fn fingers_are_their_own_bodies_not_baked_into_the_wrist() {
        // Each gripper finger is fit as its own single-hull body (in its finger
        // frame); the wrist hull covers only the wrist mesh and fixed children.
        let m = model();
        for finger in [
            "openarm_left_left_finger",
            "openarm_left_right_finger",
            "openarm_right_left_finger",
            "openarm_right_right_finger",
        ] {
            assert_eq!(
                m.local_hulls(finger)
                    .unwrap_or_else(|| panic!("finger body {finger} missing"))
                    .len(),
                1,
                "{finger} should be one auto-fit hull"
            );
        }
    }

    #[test]
    fn finger_pairs_check_across_arms_but_not_own_hand_or_sibling() {
        let m = model();
        let checked: Vec<(String, String)> = m
            .checked_pairs()
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        let has = |a: &str, b: &str| {
            checked
                .iter()
                .any(|(x, y)| (x == a && y == b) || (x == b && y == a))
        };
        // The whole point: a left finger is checked against the right gripper's
        // fingers and hand, and against the torso, so the arms cannot drive their
        // grippers into each other undetected.
        assert!(has(
            "openarm_left_right_finger",
            "openarm_right_right_finger"
        ));
        assert!(has("openarm_left_right_finger", "openarm_right_link7"));
        assert!(has("openarm_left_right_finger", "openarm_body_link0"));
        // A finger shares its wrist link's lineage, so it is not checked against
        // its own hand or its sibling finger (they touch by construction as the
        // jaws close on an object).
        assert!(!has("openarm_left_right_finger", "openarm_left_link7"));
        assert!(!has(
            "openarm_left_right_finger",
            "openarm_left_left_finger"
        ));
    }

    #[test]
    fn supplied_regions_replace_the_auto_fit() {
        let m = BimanualCollisionModel::builder(
            URDF,
            MESHES,
            "openarm_left_link0",
            "openarm_right_link0",
        )
        .regions("openarm_body_link0", covering_regions())
        .build()
        .expect("covering torso regions contain the mesh");
        assert_eq!(
            m.local_hulls("openarm_body_link0").expect("torso").len(),
            2,
            "torso uses the two supplied region pieces"
        );
    }

    #[test]
    fn rejects_regions_that_leave_mesh_uncovered() {
        // Only the lower torso is clipped in; the head vertices escape.
        let lower_only = vec![region([-INF, -INF, -INF], [INF, INF, 0.3])];
        let e = BimanualCollisionModel::builder(
            URDF,
            MESHES,
            "openarm_left_link0",
            "openarm_right_link0",
        )
        .regions("openarm_body_link0", lower_only)
        .build()
        .err()
        .expect("under-covering regions must be rejected");
        assert!(
            matches!(&e, CollisionError::Build(BuildError::HullMissesMesh { .. })),
            "{e}"
        );
    }

    #[test]
    fn rejects_a_region_that_clips_nothing() {
        // The second region floats above the whole robot: its slice of the mesh
        // is empty, which cannot bound a solid piece.
        let with_empty = vec![
            region([-INF, -INF, -INF], [INF, INF, INF]),
            region([-INF, -INF, 5.0], [INF, INF, 6.0]),
        ];
        let e = BimanualCollisionModel::builder(
            URDF,
            MESHES,
            "openarm_left_link0",
            "openarm_right_link0",
        )
        .regions("openarm_body_link0", with_empty)
        .build()
        .err()
        .expect("an empty slice must be rejected");
        assert!(
            matches!(
                &e,
                CollisionError::Build(BuildError::DegenerateRegion { index: 1, .. })
            ),
            "{e}"
        );
    }

    #[test]
    fn rejects_supplied_regions_for_unknown_body() {
        let e = BimanualCollisionModel::builder(
            URDF,
            MESHES,
            "openarm_left_link0",
            "openarm_right_link0",
        )
        .regions("no_such_body", covering_regions())
        .build()
        .err()
        .expect("unknown body must fail");
        assert!(
            matches!(
                &e,
                CollisionError::Build(BuildError::UnknownSuppliedBody { .. })
            ),
            "{e}"
        );
    }

    #[test]
    fn rejects_empty_supplied_regions() {
        let e = BimanualCollisionModel::builder(
            URDF,
            MESHES,
            "openarm_left_link0",
            "openarm_right_link0",
        )
        .regions("openarm_body_link0", Vec::new())
        .build()
        .err()
        .expect("an empty region list must fail");
        assert!(
            matches!(&e, CollisionError::Build(BuildError::EmptyRegions { .. })),
            "{e}"
        );
    }

    #[test]
    fn clearance_step_bound_dominates_the_real_change() {
        // The scan-skip soundness contract: over a joint step, the real change in
        // min surface distance never exceeds clearance_step_bound. Sampled across
        // poses (clear, in-band, near-contact), directions, magnitudes, and
        // openings; each segment is also probed at interior points, since the
        // bound must hold along the whole segment, not just at its ends.
        let mut m = model();
        let poses: [(JointVec, JointVec); 3] = [
            (
                [0.0, 0.0, 0.0, 0.05, 0.0, 0.0, 0.0],
                [0.0, 0.0, 0.0, 0.05, 0.0, 0.0, 0.0],
            ),
            (
                [0.15, 0.1, 0.85, 0.5, -0.2, 0.1, 0.0],
                [-0.05, -0.25, -0.45, 0.35, 0.1, -0.1, 0.0],
            ),
            (
                [0.0, 0.0, 0.95, 0.4, 0.1, 0.0, 0.2],
                [0.0, 0.0, -1.05, 0.4, -0.1, 0.1, 0.0],
            ),
        ];
        // A deterministic spread of step directions: single joints, all joints,
        // and mixed-sign combinations, at a small and a large magnitude.
        let dirs: Vec<(JointVec, JointVec)> = {
            let mut d: Vec<(JointVec, JointVec)> = Vec::new();
            for j in 0..ARM_DOF {
                let mut l = [0.0; ARM_DOF];
                l[j] = 1.0;
                d.push((l, [0.0; ARM_DOF]));
                d.push(([0.0; ARM_DOF], l));
            }
            d.push(([1.0; ARM_DOF], [-1.0; ARM_DOF]));
            d.push((
                std::array::from_fn(|i| if i % 2 == 0 { 1.0 } else { -1.0 }),
                std::array::from_fn(|i| if i % 3 == 0 { -1.0 } else { 1.0 }),
            ));
            d
        };
        // Opening deltas ride the same segment: the bound must dominate a step
        // that slides/swings the fingers too, not only the arm joints.
        for (open_l, open_r, dopen) in [
            (1.0, 1.0, [0.0, 0.0]),
            (0.3, 0.8, [0.7, -0.8]),
            (0.0, 0.0, [1.0, 1.0]),
        ] {
            for (ql, qr) in &poses {
                m.set_gripper_openings(open_l, open_r);
                let d0 = m.min_distance(ql, qr).expect("query").distance;
                for (dl, dr) in &dirs {
                    for mag in [0.02, 0.2] {
                        let sl: JointVec = std::array::from_fn(|i| dl[i] * mag);
                        let sr: JointVec = std::array::from_fn(|i| dr[i] * mag);
                        let bound = m.clearance_step_bound(&sl, &sr, &dopen);
                        for t in [0.25, 0.5, 1.0] {
                            let qlt: JointVec = std::array::from_fn(|i| ql[i] + t * sl[i]);
                            let qrt: JointVec = std::array::from_fn(|i| qr[i] + t * sr[i]);
                            m.set_gripper_openings(open_l + t * dopen[0], open_r + t * dopen[1]);
                            let dt = m.min_distance(&qlt, &qrt).expect("query").distance;
                            assert!(
                                (dt - d0).abs() <= bound + 1e-9,
                                "step bound violated: |{dt:+.5} - {d0:+.5}| > {bound:.5} \
                                 (mag {mag}, t {t}, dopen {dopen:?})"
                            );
                        }
                        m.set_gripper_openings(open_l, open_r);
                    }
                }
            }
        }
    }

    #[test]
    fn step_bound_is_infinite_on_non_finite_deltas() {
        // The scan-skip predicate compares `margin > bound`; a NaN bound would
        // make that false by comparison semantics alone, which a caller must not
        // have to rely on. Bad deltas must yield an explicitly infinite bound.
        let m = model();
        let mut dq = [0.0; ARM_DOF];
        dq[2] = f64::NAN;
        assert_eq!(
            m.clearance_step_bound(&dq, &[0.0; ARM_DOF], &[0.0, 0.0]),
            f64::INFINITY
        );
        assert_eq!(
            m.clearance_step_bound(&[0.0; ARM_DOF], &[0.0; ARM_DOF], &[f64::NAN, 0.0]),
            f64::INFINITY
        );
        assert_eq!(
            m.clearance_step_bound(&[0.0; ARM_DOF], &[0.0; ARM_DOF], &[0.0, f64::INFINITY]),
            f64::INFINITY
        );
    }

    #[test]
    fn broadphase_min_matches_a_brute_force_scan() {
        // The broadphase sorts pairs and stops early once the lower bound exceeds
        // the running minimum. Pin that this never changes the answer: it must
        // equal a full scan of every checked pair at every pose.
        let mut m = model();
        let pairs: Vec<(String, String)> = m
            .checked_pairs()
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        let poses = [
            ([0.0; ARM_DOF], [0.0; ARM_DOF]),
            (
                [0.0, 0.0, 1.2, 0.4, 0.0, 0.0, 0.0],
                [0.0, 0.0, -1.2, 0.4, 0.0, 0.0, 0.0],
            ),
            (
                [-0.4, -0.1, 0.0, 0.5, 0.0, -0.3, 0.0],
                [0.4, 0.1, 0.0, 0.7, 0.0, -0.2, 0.0],
            ),
        ];
        for (ql, qr) in poses {
            let fast = m.min_distance(&ql, &qr).expect("query").distance;
            let placed: HashMap<String, Vec<gjk::Hull>> = m
                .world_pieces(&ql, &qr)
                .expect("pieces")
                .into_iter()
                .map(|(name, ps)| {
                    let hulls = ps
                        .into_iter()
                        .map(|p| {
                            gjk::Hull::new(
                                &crate::hull::ConvexHull {
                                    vertices: p.vertices,
                                    faces: p.faces,
                                },
                                p.radius,
                            )
                            .expect("hull")
                        })
                        .collect();
                    (name.to_string(), hulls)
                })
                .collect();
            let slow = pairs.iter().fold(f64::INFINITY, |best, (a, b)| {
                placed[a]
                    .iter()
                    .flat_map(|ha| {
                        placed[b]
                            .iter()
                            .map(move |hb| gjk::distance(ha, hb).distance)
                    })
                    .fold(best, f64::min)
            });
            assert!(
                (fast - slow).abs() < 1e-9,
                "broadphase {fast:+.6} != brute force {slow:+.6}"
            );
        }
    }

    fn excluding(pairs: &[PairSpec]) -> Result<BimanualCollisionModel, CollisionError> {
        BimanualCollisionModel::builder(URDF, MESHES, "openarm_left_link0", "openarm_right_link0")
            .exclude(pairs)
            .build()
    }

    #[test]
    fn excludes_a_named_pair_and_reports_it() {
        let same = |a: &str, b: &str, x: &str, y: &str| (a == x && b == y) || (a == y && b == x);
        let m =
            excluding(&[PairSpec::new("openarm_left_link0", "openarm_left_link3")]).expect("model");
        assert!(
            !m.checked_pairs().iter().any(|(a, b)| same(
                a,
                b,
                "openarm_left_link0",
                "openarm_left_link3"
            )),
            "should be dropped"
        );
        assert!(
            m.excluded_pairs().iter().any(|(a, b)| same(
                a,
                b,
                "openarm_left_link0",
                "openarm_left_link3"
            )),
            "should be reported"
        );
    }

    #[test]
    fn rejects_excluding_an_unknown_body() {
        let e = excluding(&[PairSpec::new("openarm_left_link0", "no_such_link")])
            .err()
            .expect("unknown body must fail");
        assert!(
            matches!(&e, CollisionError::Build(BuildError::UnknownBody { .. })),
            "{e}"
        );
    }

    #[test]
    fn overlapping_bodies_report_negative_distance() {
        // Grippers wrapped toward each other across the torso: the winner
        // overlaps, and EPA reports a negative depth.
        let mut m = model();
        let p = m
            .min_distance(
                &[0.0, 0.0, 1.2, 0.4, 0.0, 0.0, 0.0],
                &[0.0, 0.0, -1.2, 0.4, 0.0, 0.0, 0.0],
            )
            .expect("query");
        assert!(
            p.distance < 0.0,
            "wrapped pose should overlap, got {:+.4}",
            p.distance
        );
    }

    #[test]
    fn epa_gives_continuous_signed_distance_through_overlap() {
        // EPA recovers penetration depth as a continuous signed distance: a more
        // deeply wrapped pose reads more negative than a shallower one, so a caller
        // can tell approaching from separating even from inside an overlap.
        let mut m = model();
        let deep = m
            .min_distance(
                &[0.0, 0.0, 1.2, 0.4, 0.0, 0.0, 0.0],
                &[0.0, 0.0, -1.2, 0.4, 0.0, 0.0, 0.0],
            )
            .expect("q")
            .distance;
        let shallow = m
            .min_distance(
                &[0.0, 0.0, 1.0, 0.4, 0.0, 0.0, 0.0],
                &[0.0, 0.0, -1.0, 0.4, 0.0, 0.0, 0.0],
            )
            .expect("q")
            .distance;
        assert!(
            deep < 0.0 && shallow > deep,
            "deep {deep:+.4} shallow {shallow:+.4}"
        );
    }

    #[test]
    fn rejects_non_finite_queries() {
        let mut m = model();
        let mut bad = [0.0; ARM_DOF];
        bad[0] = f64::NAN;
        assert!(m.min_distance(&bad, &[0.0; ARM_DOF]).is_err());
        assert!(m.world_pieces(&bad, &[0.0; ARM_DOF]).is_err());
    }

    #[test]
    fn model_is_send_for_task_ownership() {
        fn assert_send<T: Send>() {}
        assert_send::<BimanualCollisionModel>();
    }
}
