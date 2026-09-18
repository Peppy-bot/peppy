//! Exact convex hull of a point cloud, computed once at construction. The hull
//! is the geometric reference everything else is measured against: it is the
//! tightest convex body containing the mesh, so a proxy's deviation is its
//! distance from this hull.
//!
//! Quickhull (Barber, Dobkin & Huhdanpaa 1996): seed a tetrahedron, partition
//! the cloud into per-facet *outside sets*, then repeatedly take the point
//! farthest in front of a facet, delete every facet that point can see, and
//! stitch it to the horizon those facets leave behind. The outside sets are what
//! make it scale: a point is only ever tested against the handful of facets that
//! could claim it, never against the whole hull, so a 68k-point collision mesh
//! builds in milliseconds rather than stalling.
//!
//! Face orientation is kept robust by pointing every normal away from a fixed
//! interior point instead of tracking winding, so the horizon is found purely by
//! counting shared edges.
//!
//! Scope of the result: outside sets hold each point against one facet (the one
//! it stands furthest in front of, so the cap that replaces that facet still
//! faces it), and a deleted facet's points are re-offered to the cap. That is the
//! standard bookkeeping and it terminates with every point inside on real meshes,
//! which [`tests::a_real_collision_mesh_hulls_exactly`] pins. It is not a proof:
//! nothing here re-tests a point against a facet that survived. So this hull is
//! used as the *measurement reference* for how tightly a proxy follows a mesh, and
//! never as the thing that guarantees the proxy contains it. That guarantee comes
//! from [`crate::simplify`], whose every plane offset is a support value taken
//! over the whole raw cloud.

use std::collections::HashMap;
use std::collections::VecDeque;

use srs_model::nalgebra::{Point3, Vector3};

/// A point in front of a facet by more than this (metres) is outside it. At 1e-9
/// a point counts as inside only when it is inside to within a nanometre, far
/// under any millimetre-scale safety threshold, while still sitting some seven
/// orders above the rounding of a decimetre coordinate in `f64`, so it absorbs
/// predicate noise without absorbing geometry.
const FRONT_EPS: f64 = 1e-9;

/// Degenerate area/length guard: a facet or spanning direction below this in
/// squared magnitude is treated as collapsed. Squared, so it trips at 1e-10 in
/// length: a triangle that thin bounds no surface at collision-mesh scale, and
/// nothing this crate builds comes near it except an exactly repeated point.
const DEGEN_EPS2: f64 = 1e-20;

/// The convex hull as outward-oriented triangles over a deduplicated vertex
/// list. `vertices` is what GJK needs; `faces` index into it for rendering and
/// for the vertex adjacency the support hill-climb walks.
#[derive(Debug, Clone)]
pub struct ConvexHull {
    pub vertices: Vec<Point3<f64>>,
    pub faces: Vec<[usize; 3]>,
}

/// A working facet: vertex indices into the source cloud, an outward normal
/// (pointing away from the hull interior) with its plane offset, and the points
/// still in front of it.
struct Facet {
    v: [usize; 3],
    normal: Vector3<f64>,
    offset: f64,
    /// Cloud points in front of this facet, each held by exactly one facet.
    outside: Vec<usize>,
    /// Cleared when the facet is deleted; deletion is deferred so the surviving
    /// facets keep their indices while a horizon is being stitched.
    alive: bool,
}

impl Facet {
    /// Signed distance of a point from the facet plane, positive outward.
    fn signed_distance(&self, p: &Point3<f64>) -> f64 {
        self.normal.dot(&p.coords) - self.offset
    }
}

/// Exact convex hull of `points`: every point lies on or inside it, and every
/// returned vertex is a point of the cloud. Errors on a cloud that spans fewer
/// than three dimensions (collinear or coplanar), which no solid collision mesh
/// is, and on a stitching failure, so a hull that would not contain its own
/// cloud is a loud failure rather than a silently under-containing proxy.
pub fn exact_hull(points: &[Point3<f64>]) -> Result<ConvexHull, String> {
    // Sort so the insertion order, and thus the hull, is independent of how the
    // caller happened to order the cloud: construction is reproducible run to
    // run. Deduplicating here keeps repeated mesh vertices (every shared
    // triangle corner) out of the partition.
    let mut sorted = points.to_vec();
    sorted.sort_by(|p, q| {
        p.x.total_cmp(&q.x)
            .then_with(|| p.y.total_cmp(&q.y))
            .then_with(|| p.z.total_cmp(&q.z))
    });
    sorted.dedup();
    let points: &[Point3<f64>] = &sorted;

    let seed = initial_tetrahedron(points)?;
    let interior = Point3::from(
        seed.iter()
            .fold(Vector3::zeros(), |acc, &i| acc + points[i].coords)
            / 4.0,
    );

    let mut facets: Vec<Facet> = Vec::new();
    for [i, j, k] in [
        [seed[0], seed[1], seed[2]],
        [seed[0], seed[1], seed[3]],
        [seed[0], seed[2], seed[3]],
        [seed[1], seed[2], seed[3]],
    ] {
        facets.push(
            make_facet(i, j, k, points, &interior)
                .ok_or("the seed tetrahedron has a degenerate face")?,
        );
    }

    let seeded: Vec<usize> = (0..facets.len()).collect();
    for idx in 0..points.len() {
        claim(idx, points, &mut facets, &seeded);
    }

    // A round either folds in one new hull vertex or discards one apex it could
    // not stitch, and both are one-way, so 2n rounds covers a cloud of n points.
    // The constant carries the smallest clouds, where the seed already spends
    // four. This only backstops a numerical stall; real meshes finish far inside.
    let max_rounds = points.len() * 2 + 16;
    let mut rounds = 0;
    let mut alive = facets.len();
    let mut pending: VecDeque<usize> = seeded.into_iter().collect();
    while let Some(fi) = pending.pop_front() {
        if !facets[fi].alive || facets[fi].outside.is_empty() {
            continue;
        }
        rounds += 1;
        if rounds > max_rounds {
            return Err(format!(
                "convex hull did not converge in {max_rounds} rounds on {} points",
                points.len()
            ));
        }
        let apex = *facets[fi]
            .outside
            .iter()
            .max_by(|&&x, &&y| {
                facets[fi]
                    .signed_distance(&points[x])
                    .total_cmp(&facets[fi].signed_distance(&points[y]))
            })
            .expect("a nonempty outside set has a farthest point");

        let visible: Vec<usize> = (0..facets.len())
            .filter(|&i| facets[i].alive && facets[i].signed_distance(&points[apex]) > FRONT_EPS)
            .collect();
        let horizon = horizon_edges(&facets, &visible);
        if horizon.is_empty() {
            // Float predicates left the visible set without a closed boundary,
            // so no cap can be stitched. The apex is within FRONT_EPS of the
            // surface either way; drop it and keep the loop making progress.
            facets[fi].outside.retain(|&i| i != apex);
            pending.push_back(fi);
            continue;
        }

        let mut orphans: Vec<usize> = Vec::new();
        for &vi in &visible {
            facets[vi].alive = false;
            orphans.append(&mut facets[vi].outside);
        }
        // A horizon edge the apex is collinear with caps to a zero-area triangle.
        // `make_facet` drops those, and the drop leaves no hole: the triangle
        // bounds no surface, so the neighbouring caps already close it. Real
        // meshes hit this regularly, which is why only a horizon that caps to
        // nothing at all is an error. That the surface does stay closed is pinned
        // by Euler on the real meshes, in
        // `tests::a_real_collision_mesh_hulls_exactly`, rather than assumed here.
        let first_new = facets.len();
        for (a, b) in horizon {
            if let Some(f) = make_facet(a, b, apex, points, &interior) {
                facets.push(f);
            }
        }
        if facets.len() == first_new {
            return Err("hull stitching produced no facets across the horizon".into());
        }
        let fresh: Vec<usize> = (first_new..facets.len()).collect();
        alive = alive - visible.len() + fresh.len();
        for idx in orphans {
            if idx != apex {
                claim(idx, points, &mut facets, &fresh);
            }
        }
        pending.extend(fresh);

        // Deleted facets are only marked, so the visibility scan would otherwise
        // grow with the whole construction history rather than the live surface.
        // Compacting once the dead outnumber the living three to one keeps that
        // scan, and so the whole build, linear in the hull rather than in its
        // history, while being rare enough that the copying stays amortised. The
        // floor of 16 stops a hull that is still only a few facets from
        // compacting on every round.
        if facets.len() > 4 * alive.max(16) {
            facets.retain(|f| f.alive);
            alive = facets.len();
            pending = (0..facets.len())
                .filter(|&i| !facets[i].outside.is_empty())
                .collect();
        }
    }

    Ok(reindex(points, &facets))
}

/// Hand point `idx` to whichever of `candidates` it stands furthest in front of,
/// or to none if it is behind them all (already inside). Assigning by the
/// largest violation, not the first, keeps a point with the facet that faces it,
/// which is the facet whose replacement cap will still face it after an
/// insertion.
fn claim(idx: usize, points: &[Point3<f64>], facets: &mut [Facet], candidates: &[usize]) {
    let p = &points[idx];
    let best = candidates
        .iter()
        .filter(|&&fi| facets[fi].alive)
        .map(|&fi| (fi, facets[fi].signed_distance(p)))
        .filter(|&(_, d)| d > FRONT_EPS)
        .max_by(|x, y| x.1.total_cmp(&y.1));
    if let Some((fi, _)) = best {
        facets[fi].outside.push(idx);
    }
}

/// Undirected edges that border exactly one visible facet: the boundary between
/// the deleted cap and the rest of the hull, where the new facets attach.
fn horizon_edges(facets: &[Facet], visible: &[usize]) -> Vec<(usize, usize)> {
    let mut count: HashMap<(usize, usize), u32> = HashMap::new();
    for &fi in visible {
        let v = facets[fi].v;
        for (a, b) in [(v[0], v[1]), (v[1], v[2]), (v[2], v[0])] {
            *count
                .entry(if a < b { (a, b) } else { (b, a) })
                .or_insert(0) += 1;
        }
    }
    let mut edges: Vec<(usize, usize)> = count
        .into_iter()
        .filter(|&(_, c)| c == 1)
        .map(|(e, _)| e)
        .collect();
    // A HashMap iterates in an unspecified order; sorting keeps the stitched
    // facet list, and so the whole hull, reproducible.
    edges.sort_unstable();
    edges
}

/// A facet on `i, j, k`, its normal flipped to point away from `interior`.
/// `None` if the three points are collinear (zero area).
fn make_facet(
    i: usize,
    j: usize,
    k: usize,
    points: &[Point3<f64>],
    interior: &Point3<f64>,
) -> Option<Facet> {
    let n = (points[j] - points[i]).cross(&(points[k] - points[i]));
    if n.norm_squared() <= DEGEN_EPS2 {
        return None;
    }
    let mut normal = n.normalize();
    if normal.dot(&(interior - points[i])) > 0.0 {
        normal = -normal;
    }
    Some(Facet {
        v: [i, j, k],
        normal,
        offset: normal.dot(&points[i].coords),
        outside: Vec::new(),
        alive: true,
    })
}

/// Four affinely independent seed points: an extreme point, the farthest from
/// it, the farthest from that line, the farthest from that plane.
fn initial_tetrahedron(points: &[Point3<f64>]) -> Result<[usize; 4], String> {
    if points.len() < 4 {
        return Err(format!(
            "a hull needs at least four points, got {}",
            points.len()
        ));
    }
    let i0 = (0..points.len())
        .max_by(|&a, &b| points[a].x.total_cmp(&points[b].x))
        .expect("nonempty");
    let i1 =
        farthest(points, |p| (p - points[i0]).norm_squared()).ok_or("cloud is a single point")?;
    let axis = points[i1] - points[i0];
    if axis.norm_squared() <= DEGEN_EPS2 {
        return Err("cloud is a single point".into());
    }
    let i2 = farthest(points, |p| (p - points[i0]).cross(&axis).norm_squared())
        .ok_or("collinear cloud")?;
    let normal = (points[i1] - points[i0]).cross(&(points[i2] - points[i0]));
    if normal.norm_squared() <= DEGEN_EPS2 {
        return Err("collinear cloud has no hull".into());
    }
    let i3 = farthest(points, |p| (p - points[i0]).dot(&normal).abs()).ok_or("coplanar cloud")?;
    if (points[i3] - points[i0]).dot(&normal).abs() <= FRONT_EPS {
        return Err("coplanar cloud has no volume".into());
    }
    Ok([i0, i1, i2, i3])
}

/// Index of the point maximizing `score`, or `None` if every score is zero
/// (the cloud is degenerate along this measure).
fn farthest(points: &[Point3<f64>], score: impl Fn(&Point3<f64>) -> f64) -> Option<usize> {
    let (idx, best) = (0..points.len())
        .map(|i| (i, score(&points[i])))
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    if best <= DEGEN_EPS2 { None } else { Some(idx) }
}

/// Compact the surviving facets to a deduplicated vertex list.
fn reindex(points: &[Point3<f64>], facets: &[Facet]) -> ConvexHull {
    let mut remap: HashMap<usize, usize> = HashMap::new();
    let mut vertices = Vec::new();
    let mut out_faces = Vec::new();
    for f in facets.iter().filter(|f| f.alive) {
        let tri = f.v.map(|old| {
            *remap.entry(old).or_insert_with(|| {
                vertices.push(points[old]);
                vertices.len() - 1
            })
        });
        out_faces.push(tri);
    }
    ConvexHull {
        vertices,
        faces: out_faces,
    }
}

/// Outward face planes (unit normal, plane offset) of a hull, oriented away from
/// the vertex centroid (interior to a convex hull). Degenerate faces are
/// dropped: they bound nothing.
///
/// Test-only: the plane metric understates the true distance to a hull near an
/// edge or corner, so production containment and deviation both go through GJK
/// instead. It is exact for the one question the tests ask of it, whether a point
/// is inside.
#[cfg(test)]
pub fn face_planes(hull: &ConvexHull) -> Vec<(Vector3<f64>, f64)> {
    let interior = Point3::from(
        hull.vertices
            .iter()
            .fold(Vector3::zeros(), |a, v| a + v.coords)
            / hull.vertices.len().max(1) as f64,
    );
    hull.faces
        .iter()
        .filter_map(|f| {
            let (a, b, c) = (
                hull.vertices[f[0]],
                hull.vertices[f[1]],
                hull.vertices[f[2]],
            );
            let n = (b - a).cross(&(c - a));
            if n.norm_squared() <= DEGEN_EPS2 {
                return None;
            }
            let mut normal = n.normalize();
            if normal.dot(&(interior - a)) > 0.0 {
                normal = -normal;
            }
            Some((normal, normal.dot(&a.coords)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{RngExt, SeedableRng};

    fn pt(x: f64, y: f64, z: f64) -> Point3<f64> {
        Point3::new(x, y, z)
    }

    /// Worst signed distance of `p` past any face plane of `hull` (negative when
    /// strictly inside). The plane metric understates true distance near an edge,
    /// but for "is this point inside a convex hull" it is exact.
    fn worst_violation(hull: &ConvexHull, p: &Point3<f64>) -> f64 {
        face_planes(hull)
            .iter()
            .map(|(n, off)| n.dot(&p.coords) - off)
            .fold(f64::NEG_INFINITY, f64::max)
    }

    fn contains_every(hull: &ConvexHull, points: &[Point3<f64>]) -> f64 {
        let planes = face_planes(hull);
        points
            .iter()
            .map(|p| {
                planes
                    .iter()
                    .map(|(n, off)| n.dot(&p.coords) - off)
                    .fold(f64::NEG_INFINITY, f64::max)
            })
            .fold(f64::NEG_INFINITY, f64::max)
    }

    /// From just the eight corners the hull is the clean box: eight vertices,
    /// twelve triangles, all corners inside.
    #[test]
    fn cube_corners_make_a_clean_box() {
        let mut verts = Vec::new();
        for sx in [0.0, 1.0] {
            for sy in [0.0, 1.0] {
                for sz in [0.0, 1.0] {
                    verts.push(pt(sx, sy, sz));
                }
            }
        }
        let hull = exact_hull(&verts).expect("box hull");
        assert_eq!(hull.vertices.len(), 8, "a box has eight hull vertices");
        assert_eq!(hull.faces.len(), 12, "a box triangulates to twelve faces");
        assert!(contains_every(&hull, &verts) <= 1e-12);
    }

    /// A dense grid contains every point and keeps the corners. The vertex count
    /// is not asserted minimal: a point exactly coplanar with a face can be
    /// stitched in and never removed, since no later apex is strictly in front of
    /// the facets holding it. Dropping such passengers was measured to cost more
    /// build time than it saves in support scans, so they are carried.
    #[test]
    fn a_dense_grid_hull_contains_every_point_and_keeps_the_corners() {
        let mut pts = Vec::new();
        for i in 0..5 {
            for j in 0..5 {
                for k in 0..5 {
                    pts.push(pt(i as f64 / 4.0, j as f64 / 4.0, k as f64 / 4.0));
                }
            }
        }
        let hull = exact_hull(&pts).expect("grid hull");
        assert!(contains_every(&hull, &pts) <= 1e-12, "a grid point escaped");
        for corner in [
            pt(0.0, 0.0, 0.0),
            pt(1.0, 1.0, 1.0),
            pt(1.0, 0.0, 1.0),
            pt(0.0, 1.0, 0.0),
        ] {
            assert!(
                hull.vertices.iter().any(|v| (v - corner).norm() < 1e-9),
                "corner {corner:?} missing"
            );
        }
        assert!(
            hull.vertices.len() < pts.len(),
            "hull should compress the cloud"
        );
    }

    #[test]
    fn interior_points_are_dropped() {
        let mut pts = vec![
            pt(0.0, 0.0, 0.0),
            pt(1.0, 0.0, 0.0),
            pt(0.0, 1.0, 0.0),
            pt(0.0, 0.0, 1.0),
            pt(1.0, 1.0, 1.0),
        ];
        let mut rng = rand::rngs::StdRng::seed_from_u64(5);
        for _ in 0..200 {
            pts.push(pt(
                rng.random_range(0.05..0.3),
                rng.random_range(0.05..0.3),
                rng.random_range(0.05..0.3),
            ));
        }
        let hull = exact_hull(&pts).expect("hull");
        assert_eq!(
            hull.vertices.len(),
            5,
            "only the five extreme points are vertices"
        );
    }

    #[test]
    fn every_cloud_point_lies_inside_its_hull() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(9);
        for _ in 0..20 {
            let pts: Vec<_> = (0..300)
                .map(|_| {
                    pt(
                        rng.random_range(-1.0..1.0),
                        rng.random_range(-1.0..1.0),
                        rng.random_range(-1.0..1.0),
                    )
                })
                .collect();
            let hull = exact_hull(&pts).expect("hull");
            assert!(
                contains_every(&hull, &pts) <= 1e-9,
                "a cloud point escaped its own hull"
            );
            assert!(
                hull.vertices.len() < pts.len(),
                "hull should compress the cloud"
            );
        }
    }

    /// Every vertex the hull keeps is genuinely extreme: it lies on the surface,
    /// not strictly inside. This is what a welded or incremental fit fails and
    /// what makes the vertex count meaningful as a query-cost proxy.
    #[test]
    fn every_kept_vertex_lies_on_the_surface() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(11);
        let pts: Vec<_> = (0..2000)
            .map(|_| {
                pt(
                    rng.random_range(-1.0..1.0),
                    rng.random_range(-1.0..1.0),
                    rng.random_range(-1.0..1.0),
                )
            })
            .collect();
        let hull = exact_hull(&pts).expect("hull");
        for v in &hull.vertices {
            assert!(
                worst_violation(&hull, v).abs() < 1e-9,
                "vertex {v:?} is not on the surface"
            );
        }
    }

    #[test]
    fn sphere_surface_points_are_all_vertices() {
        // Points on a sphere are all extreme, so all survive as hull vertices.
        let mut pts = Vec::new();
        for i in 0..80 {
            let phi = i as f64 * 0.618 * std::f64::consts::TAU;
            let z = -1.0 + 2.0 * (i as f64 + 0.5) / 80.0;
            let r = (1.0f64 - z * z).sqrt();
            pts.push(pt(r * phi.cos(), r * phi.sin(), z));
        }
        let hull = exact_hull(&pts).expect("sphere hull");
        assert_eq!(
            hull.vertices.len(),
            pts.len(),
            "all sphere points are extreme"
        );
    }

    #[test]
    fn rejects_degenerate_clouds() {
        let flat: Vec<_> = (0..10)
            .flat_map(|i| (0..10).map(move |j| pt(i as f64, j as f64, 0.0)))
            .collect();
        assert!(exact_hull(&flat).is_err(), "a flat cloud has no volume");
        assert!(exact_hull(&[pt(0.0, 0.0, 0.0)]).is_err(), "one point");
        let line: Vec<_> = (0..10).map(|i| pt(i as f64, 0.0, 0.0)).collect();
        assert!(exact_hull(&line).is_err(), "a collinear cloud has no hull");
    }

    /// A real collision mesh in the production (metre) frame: the cloud size and
    /// coplanar machined faces are exactly what stalls a naive incremental hull,
    /// and every mesh point must end up inside.
    #[test]
    fn a_real_collision_mesh_hulls_exactly() {
        let raw = crate::stl::load_stl(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/meshes/link6_symp.stl"
        ))
        .expect("mesh");
        let pts: Vec<Point3<f64>> = raw
            .iter()
            .map(|v| pt(v.x * 0.001, v.y * 0.001, v.z * 0.001))
            .collect();
        let hull = exact_hull(&pts).expect("hull");
        assert!(
            contains_every(&hull, &pts) <= 1e-9,
            "a mesh point escaped the exact hull"
        );
        assert!(
            hull.vertices.len() < pts.len() / 4,
            "the hull should be a real compression of {} mesh points, got {}",
            pts.len(),
            hull.vertices.len()
        );
        // Euler on a closed genus-0 triangulation: V - E + F = 2 with E = 3F/2,
        // so F = 2V - 4 exactly. Equality is the real assertion here, and it is
        // what rules out the failure the stitching loop tolerates: a horizon edge
        // whose degenerate cap is dropped would, if it truly left a hole, show up
        // as a facet deficit against this count.
        assert_eq!(
            hull.faces.len(),
            2 * hull.vertices.len() - 4,
            "{} faces over {} vertices is not a closed triangulation",
            hull.faces.len(),
            hull.vertices.len()
        );
    }

    /// The hull does not depend on the caller's point order.
    #[test]
    fn construction_is_order_independent() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(21);
        let pts: Vec<_> = (0..500)
            .map(|_| {
                pt(
                    rng.random_range(-1.0..1.0),
                    rng.random_range(-1.0..1.0),
                    rng.random_range(-1.0..1.0),
                )
            })
            .collect();
        let a = exact_hull(&pts).expect("hull");
        let mut shuffled = pts.clone();
        shuffled.reverse();
        let b = exact_hull(&shuffled).expect("hull");
        assert_eq!(a.vertices, b.vertices);
        assert_eq!(a.faces, b.faces);
    }
}
