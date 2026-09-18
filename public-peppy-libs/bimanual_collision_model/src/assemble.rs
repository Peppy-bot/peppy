//! Fit the collision bodies for a bimanual model directly from the URDF and
//! its meshes, at construction time. There is no intermediate artifact: the
//! hulls can never go stale against the geometry.

use std::collections::{HashMap, HashSet};

use srs_model::nalgebra::Point3;

use crate::clip::ClipRegion;
use crate::gjk::{self, Hull};
use crate::hull::ConvexHull;
use crate::simplify::circumscribe;
use crate::urdf_collision::{ParentJoint, UrdfCollisions};
use crate::{BuildError, ContainmentFailure};

/// How much slack a fitted piece may carry: the widest gap (metres) from its own
/// surface inward to the mesh's convex hull.
///
/// A piece always encloses its mesh, at any budget, because every plane of the
/// fit is a supporting plane of the mesh cloud. So this is never a leak; it is
/// the opposite, the worst amount by which a piece is *larger* than the geometry
/// it stands in for. That excess is clearance the arms give away, since the
/// governor sees the piece and not the part, which is why it sits an order of
/// magnitude under the millimetre-scale bands tuned over it. The face count
/// needed to hold it falls out of how much the surface curves.
const MAX_DEVIATION_M: f64 = 0.001;
/// A mesh point counts as inside a supplied hull when its signed distance is
/// within this of the surface (metres), so on-boundary points pass.
const CONTAIN_TOL: f64 = 1e-6;
/// Subdivision floor for the face-coverage check (metres, longest edge): a
/// sub-triangle smaller than this that still sits in no single piece is reported
/// as escaping. Well below the centimetre-scale proximity bands the model feeds.
const FACE_FLOOR: f64 = 1e-3;
/// A face whose smallest altitude is below this (metres) is a degenerate sliver
/// from mesh simplification: it bounds no surface, so the face check skips it.
/// Its corners are still checked by the per-vertex pass.
const SLIVER_ALTITUDE: f64 = 1e-4;

/// Convex-hull pieces for every collision body: world-fixed bodies (in root
/// frame), chain links (in link frame, with any *fixed* collision-bearing child
/// baked in), and the movable end-effector fingers, each fit as its own body in
/// its finger-link frame and placed live from the gripper opening. Each body is
/// one auto-fit hull unless the caller supplied its own pieces.
pub(crate) struct FittedBodies {
    pub fixed: Vec<(String, Vec<Hull>)>,
    pub links: HashMap<String, Vec<Hull>>,
    pub fingers: Vec<FittedFinger>,
}

/// A gripper finger: a collision-bearing link on a movable joint off a chain
/// link. Fit as its own hull (not baked into the parent) so it can be placed at
/// the live opening rather than swept over its whole travel. `parent_link` is the
/// chain link it hangs off (its FK segment host); `joint` places its hull in that
/// link's frame at a given opening ([`ParentJoint::offset`]). `closed` and `open`
/// are the joint positions of the two travel extremes, oriented geometrically by
/// [`orient_finger_pair`], not by the URDF limit order.
pub(crate) struct FittedFinger {
    pub name: String,
    pub parent_link: String,
    pub hulls: Vec<Hull>,
    pub joint: ParentJoint,
    pub closed: f64,
    pub open: f64,
}

/// Fit all collision bodies. `chains` are the moving-link names per arm (from
/// FK); every other collision-bearing link must be world-fixed or an attached
/// child of a chain link, anything else is an error rather than a silently
/// unmodeled body. `supplied` overrides the auto-fit for named bodies with a
/// clip-region decomposition, whose fitted pieces are verified here to contain
/// that body's mesh.
pub(crate) fn fit_bodies(
    urdf: &UrdfCollisions,
    chains: &[Vec<String>],
    meshes_dir: &str,
    supplied: &HashMap<String, Vec<ClipRegion>>,
) -> Result<FittedBodies, BuildError> {
    let chain_set: HashSet<&str> = chains.iter().flatten().map(String::as_str).collect();
    let mut body_names: HashSet<String> = HashSet::new();

    let mut links = HashMap::new();
    let mut fingers: Vec<FittedFinger> = Vec::new();
    let mut attached: HashSet<String> = HashSet::new();
    for name in chains.iter().flatten() {
        // A chain link's cloud is its mesh plus any *fixed* collision-bearing
        // child (a fixed sensor or hand base rides rigidly with the link). A
        // *movable* collision-bearing child is a gripper finger: its own body,
        // fit from its own mesh and placed live at the current opening; the
        // link hull covers only the link mesh and its fixed children. Only
        // direct children are handled; a deeper collision-bearing descendant
        // (a multi-link end-effector) falls through to the fixed-body pass
        // below, which errors if it is not actually world-fixed, so such a
        // URDF fails construction loudly rather than being modeled wrong.
        // OpenArm has only single-link attachments; widen this to a recursive
        // walk if a multi-link end-effector is added. A mimic finger joint's
        // own declared limits are its travel; the mimic multiplier/offset are
        // not resolved, sound while those limits cover the true travel
        // (OpenArm's fingers mirror 1:1).
        let mut verts = urdf.link_vertices(name, meshes_dir)?;
        let mut link_fingers: Vec<(FittedFinger, Point3<f64>)> = Vec::new();
        for child in urdf.children_of(name) {
            if chain_set.contains(child.as_str()) || urdf.collisions_of(&child).is_empty() {
                continue;
            }
            let joint = urdf
                .parent_joint(&child)
                .expect("children_of implies a parent joint")
                .clone();
            attached.insert(child.clone());
            if joint.is_fixed() {
                verts.extend(urdf.child_vertices_in_parent(&child, 0.0, meshes_dir)?);
                continue;
            }
            // A movable finger placed live per opening. An unplaceable joint
            // kind (continuous/planar/floating) is rejected when the model
            // parses this into its runtime placer, so the build still fails
            // loudly there; the mesh centroid feeds the pair orientation below.
            let finger_verts = urdf.link_vertices(&child, meshes_dir)?;
            let centroid = vertex_centroid(&finger_verts);
            let hulls = fit_body(&child, &finger_verts, supplied)?;
            body_names.insert(child.clone());
            let (closed, open) = (joint.lower_limit, joint.upper_limit);
            link_fingers.push((
                FittedFinger {
                    name: child,
                    parent_link: name.clone(),
                    hulls,
                    joint,
                    closed,
                    open,
                },
                centroid,
            ));
        }
        orient_finger_pair(&mut link_fingers)?;
        fingers.extend(link_fingers.into_iter().map(|(finger, _)| finger));
        links.insert(name.clone(), fit_body(name, &verts, supplied)?);
        body_names.insert(name.clone());
    }

    let mut fixed = Vec::new();
    for name in urdf.collision_link_names() {
        if chain_set.contains(name.as_str()) || attached.contains(&name) {
            continue;
        }
        let verts = urdf
            .fixed_vertices_in_root(&name, meshes_dir)
            .map_err(|e| format!("collision link '{name}' is neither a chain link, an attached child, nor world-fixed: {e}"))?;
        let hulls = fit_body(&name, &verts, supplied)?;
        body_names.insert(name.clone());
        fixed.push((name, hulls));
    }

    if let Some(unknown) = supplied.keys().find(|k| !body_names.contains(*k)) {
        return Err(BuildError::UnknownSuppliedBody {
            name: unknown.clone(),
        });
    }

    Ok(FittedBodies {
        fixed,
        links,
        fingers,
    })
}

/// Orient a two-finger gripper's `closed`/`open` joint positions from its
/// meshes. URDF convention puts closed at the lower limit, but a mirrored
/// gripper (OpenArm v2's right hand) mirrors by flipping the limit range
/// instead of the axis, so its lower limit is the open end; trusting the
/// convention there would invert the live placement, and "fully open" (the
/// conservative default) would park the collision fingers closed. The meshes
/// decide instead: the travel extreme with the larger between-finger centroid
/// separation is open. The limit order is untrustworthy in general, so a link
/// with any other movable-child count has no oriented travel and errors,
/// keeping an unorientable gripper a loud build failure instead of a silently
/// inverted model.
fn orient_finger_pair(pair: &mut [(FittedFinger, Point3<f64>)]) -> Result<(), BuildError> {
    let [a, b] = pair else {
        if pair.is_empty() {
            return Ok(());
        }
        return Err(BuildError::Geometry(format!(
            "link '{}' has {} movable collision-bearing children; orienting finger travel \
             needs exactly two (a two-finger gripper)",
            pair[0].0.parent_link,
            pair.len()
        )));
    };
    let posed = |f: &(FittedFinger, Point3<f64>), q: f64| -> Result<Point3<f64>, BuildError> {
        let offset =
            f.0.joint
                .offset(q)
                .map_err(|e| BuildError::Geometry(format!("finger '{}': {e}", f.0.name)))?;
        Ok(offset * f.1)
    };
    let separation = |qa: f64, qb: f64| -> Result<f64, BuildError> {
        Ok((posed(a, qa)? - posed(b, qb)?).norm())
    };
    let at_lower = separation(a.0.joint.lower_limit, b.0.joint.lower_limit)?;
    let at_upper = separation(a.0.joint.upper_limit, b.0.joint.upper_limit)?;
    // A real gripper's tip separation differs by centimetres between its limit
    // extremes; near-equal separations mean the two joints' limit ranges point
    // opposite ways (e.g. a half-mirrored export), where a strict comparison
    // would pick an orientation by floating-point noise. Refuse to guess.
    const ORIENT_MARGIN_M: f64 = 1e-6;
    if (at_lower - at_upper).abs() <= ORIENT_MARGIN_M {
        return Err(BuildError::Geometry(format!(
            "link '{}': finger travel orientation is ambiguous (tip separation \
             {at_lower:.6} m at the lower limits vs {at_upper:.6} m at the upper); \
             the two finger joints' limit ranges do not agree on a closed-to-open \
             direction",
            a.0.parent_link
        )));
    }
    if at_lower > at_upper {
        for (finger, _) in pair.iter_mut() {
            (finger.closed, finger.open) = (finger.joint.upper_limit, finger.joint.lower_limit);
        }
    }
    Ok(())
}

/// Centroid of a vertex cloud (the meshes here are dense enough that the
/// vertex mean is a stable stand-in for the surface centroid).
fn vertex_centroid(verts: &[Point3<f64>]) -> Point3<f64> {
    let sum = verts
        .iter()
        .fold(srs_model::nalgebra::Vector3::zeros(), |acc, p| {
            acc + p.coords
        });
    Point3::from(sum / verts.len().max(1) as f64)
}

/// The hulls for one body. Without an override: a single circumscribing fit of
/// the vertex cloud. With supplied clip regions (a concave body): the mesh
/// surface is clipped to each region and every clipped slice gets the same fit,
/// so the pieces track the mesh exactly as the links do, then the union is
/// verified to contain the mesh.
///
/// The fit's planes support the mesh, so each piece contains its own cloud by
/// construction and the pieces carry no inflation radius: the reported distance
/// is the surface clearance less at most [`MAX_DEVIATION_M`], not one shrunk by a
/// sweep around a shape too coarse to follow the mesh.
fn fit_body(
    name: &str,
    verts: &[Point3<f64>],
    supplied: &HashMap<String, Vec<ClipRegion>>,
) -> Result<Vec<Hull>, BuildError> {
    let Some(regions) = supplied.get(name) else {
        let fit =
            circumscribe(verts, MAX_DEVIATION_M).map_err(|reason| BuildError::DegenerateBody {
                body: name.to_string(),
                reason,
            })?;
        return Ok(vec![Hull::new(&fit.hull, 0.0)?]);
    };
    if regions.is_empty() {
        return Err(BuildError::EmptyRegions {
            body: name.to_string(),
        });
    }
    let mut hulls = Vec::with_capacity(regions.len());
    for (index, region) in regions.iter().enumerate() {
        let points = region.clip_triangles(verts);
        let fit = circumscribe(&points, MAX_DEVIATION_M).map_err(|reason| {
            BuildError::DegenerateRegion {
                body: name.to_string(),
                index,
                reason,
            }
        })?;
        hulls.push(Hull::new(&fit.hull, 0.0)?);
    }
    verify_contains(name, &hulls, verts)?;
    Ok(hulls)
}

/// Error unless the supplied hulls conservatively contain the body's whole mesh
/// surface, not merely its vertices. `verts` is the mesh triangle soup (every
/// three a triangle). A vertex-only check passes geometry that leaks: a face can
/// slope through the gap between two pieces while its three corners land inside
/// them (a tapering shoulder against stacked boxes). So every real face is
/// checked for surface coverage, and every vertex is checked outright (the
/// latter also covers the corners of degenerate sliver faces, which the face
/// pass skips because they bound no surface).
fn verify_contains(name: &str, hulls: &[Hull], verts: &[Point3<f64>]) -> Result<(), BuildError> {
    for v in verts {
        if !inside_union(hulls, v)? {
            return Err(BuildError::HullMissesMesh {
                body: name.to_string(),
                kind: ContainmentFailure::VertexOutside,
            });
        }
    }
    for tri in verts.chunks_exact(3) {
        let t = [tri[0], tri[1], tri[2]];
        if min_altitude(&t) < SLIVER_ALTITUDE {
            continue;
        }
        if !face_in_union(hulls, &t)? {
            return Err(BuildError::HullMissesMesh {
                body: name.to_string(),
                kind: ContainmentFailure::FaceEscapes,
            });
        }
    }
    Ok(())
}

/// Whether `v` lies inside any single hull (within [`CONTAIN_TOL`]).
fn inside_union(hulls: &[Hull], v: &Point3<f64>) -> Result<bool, String> {
    for h in hulls {
        if inside(h, v)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether every point of triangle `t` lies in the union of the hulls. A
/// triangle whose three corners share one convex hull lies wholly in it (a
/// convex set contains every convex combination of its points). One that
/// straddles pieces is split into four and each part re-checked, down to a
/// [`FACE_FLOOR`] edge length below which an uncovered remnant is reported as
/// escaping. The recursion only deepens near piece boundaries: a sub-triangle
/// that falls entirely in one piece stops immediately.
fn face_in_union(hulls: &[Hull], t: &[Point3<f64>; 3]) -> Result<bool, String> {
    for h in hulls {
        if t.iter()
            .try_fold(true, |acc, v| Ok::<_, String>(acc && inside(h, v)?))?
        {
            return Ok(true);
        }
    }
    if longest_edge(t) < FACE_FLOOR {
        return Ok(false);
    }
    let mid = |a: &Point3<f64>, b: &Point3<f64>| Point3::from((a.coords + b.coords) / 2.0);
    let (m01, m12, m20) = (mid(&t[0], &t[1]), mid(&t[1], &t[2]), mid(&t[2], &t[0]));
    let subs = [
        [t[0], m01, m20],
        [m01, t[1], m12],
        [m20, m12, t[2]],
        [m01, m12, m20],
    ];
    for s in &subs {
        if !face_in_union(hulls, s)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether point `v` lies inside hull `h` (within [`CONTAIN_TOL`]).
fn inside(h: &Hull, v: &Point3<f64>) -> Result<bool, String> {
    let point = Hull::new(
        &ConvexHull {
            vertices: vec![*v],
            faces: Vec::new(),
        },
        0.0,
    )?;
    Ok(gjk::distance(&point, h).distance <= CONTAIN_TOL)
}

/// The longest of the triangle's three edges (metres).
fn longest_edge(t: &[Point3<f64>; 3]) -> f64 {
    [
        (t[1] - t[0]).norm(),
        (t[2] - t[1]).norm(),
        (t[0] - t[2]).norm(),
    ]
    .into_iter()
    .fold(0.0, f64::max)
}

/// The triangle's smallest altitude (`2 * area / longest edge`): near zero for a
/// degenerate sliver, which has no surface to contain.
fn min_altitude(t: &[Point3<f64>; 3]) -> f64 {
    let area = 0.5 * (t[1] - t[0]).cross(&(t[2] - t[0])).norm();
    let longest = longest_edge(t);
    if longest <= 0.0 {
        0.0
    } else {
        2.0 * area / longest
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hull::exact_hull;

    /// A solid box hull (radius 0) from its min/max corner.
    fn box_hull(min: [f64; 3], max: [f64; 3]) -> Hull {
        let corners: Vec<Point3<f64>> = (0..8)
            .map(|i| {
                let pick = |bit: usize, c: usize| if i & (1 << bit) == 0 { min[c] } else { max[c] };
                Point3::new(pick(0, 0), pick(1, 1), pick(2, 2))
            })
            .collect();
        Hull::new(&exact_hull(&corners).expect("box hull"), 0.0).expect("hull")
    }

    /// Two boxes with an empty gap in z, the case a vertex-only check gets wrong.
    fn split_pieces() -> [Hull; 2] {
        [
            box_hull([-0.05, -0.05, 0.0], [0.05, 0.05, 0.1]),
            box_hull([-0.05, -0.05, 0.2], [0.05, 0.05, 0.3]),
        ]
    }

    #[test]
    fn a_real_face_bridging_the_gap_is_rejected() {
        // Corners land in the two pieces, but the face slopes through the gap.
        let verts = vec![
            Point3::new(0.0, 0.0, 0.05),
            Point3::new(0.04, 0.0, 0.25),
            Point3::new(-0.04, 0.0, 0.25),
        ];
        let e = verify_contains("t", &split_pieces(), &verts)
            .expect_err("bridging face must be rejected");
        assert!(
            matches!(
                &e,
                BuildError::HullMissesMesh {
                    kind: ContainmentFailure::FaceEscapes,
                    ..
                }
            ),
            "{e}"
        );
    }

    #[test]
    fn a_degenerate_sliver_across_the_gap_is_skipped() {
        // Collinear: no surface to contain, only its (contained) corners matter.
        let verts = vec![
            Point3::new(0.0, 0.0, 0.05),
            Point3::new(0.0, 0.0, 0.25),
            Point3::new(0.0, 0.0, 0.28),
        ];
        verify_contains("t", &split_pieces(), &verts)
            .expect("a sliver is skipped when its corners are contained");
    }

    #[test]
    fn a_vertex_outside_every_piece_is_rejected() {
        let verts = vec![
            Point3::new(0.0, 0.0, 0.05),
            Point3::new(0.0, 0.0, 0.15),
            Point3::new(0.0, 0.0, 0.25),
        ];
        let e = verify_contains("t", &split_pieces(), &verts)
            .expect_err("a gap vertex must be rejected");
        assert!(
            matches!(
                &e,
                BuildError::HullMissesMesh {
                    kind: ContainmentFailure::VertexOutside,
                    ..
                }
            ),
            "{e}"
        );
    }

    #[test]
    fn a_face_inside_one_piece_passes() {
        let tall = [box_hull([-0.05, -0.05, 0.0], [0.05, 0.05, 0.3])];
        let verts = vec![
            Point3::new(0.0, 0.0, 0.05),
            Point3::new(0.04, 0.0, 0.25),
            Point3::new(-0.04, 0.0, 0.25),
        ];
        verify_contains("t", &tall, &verts).expect("a face within one piece is contained");
    }
}
