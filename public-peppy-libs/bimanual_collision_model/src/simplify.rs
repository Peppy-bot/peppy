//! Fit a mesh with a small polytope that circumscribes it.
//!
//! Every plane of the fit is a *supporting* plane of the mesh cloud: its offset
//! is the cloud's own support value in that direction, so the halfspace holds the
//! whole mesh and the intersection of any number of them does too. Containment is
//! therefore structural, not something a sweep radius has to rescue afterwards,
//! and the fit reports a true clearance instead of one shrunk by an inflation.
//!
//! The plane set is chosen by estimate refinement (Kamenev 1992), the cutting
//! scheme for outer polyhedral approximation of a convex body: start from the
//! cloud's axis-aligned bounding box (six supporting planes, bounded by
//! construction), then repeatedly take the vertex of the current polytope that
//! sits furthest outside the mesh's convex hull and cut it off with the
//! supporting plane whose normal points from the hull to that vertex. That plane
//! touches the hull exactly at the vertex's nearest point, so it drives that
//! deviation to zero, and the loop stops once the worst remaining deviation is
//! inside the caller's budget. Faces land where the surface curves and nowhere
//! else: a flat needs one plane however large it is, and the scheme is known to
//! grow vertices and facets at the optimal order in the approximation error.
//!
//! Refining outward to an error budget is chosen over decimating the exact hull
//! down to a face count, which is the other standard route. The budget is a
//! Hausdorff distance, so it bounds the quantity a proximity governor is exposed
//! to; minimising added volume for a fixed face count does not, and can spend its
//! whole error on one spike.
//!
//! Deviation is measured as true Euclidean distance to the hull, never as a
//! face-plane violation. The plane metric understates the distance from a point
//! off an edge or a corner (by `1/sqrt(2)` at a right angle), so budgeting
//! against it would quietly overshoot.
//!
//! G. K. Kamenev, "A class of adaptive algorithms for approximation of convex
//! bodies by polyhedra", Zh. Vychisl. Mat. Mat. Fiz. 32(1):136-152, 1992;
//! translated in Computational Mathematics and Mathematical Physics 32:114-127.

use std::cell::Cell;

use srs_model::nalgebra::{Point3, Unit, Vector3};

use crate::gjk::{self, Support};
use crate::hull::{ConvexHull, exact_hull};

/// Halfspace count at which the fit gives up. A budget this many supporting
/// planes cannot meet means a pathologically round body, and erroring beats
/// silently shipping a proxy looser than the caller asked for.
///
/// The worst of the eleven collision meshes converges in 75 planes at a 1 mm
/// budget, so this leaves roughly sevenfold headroom: loose enough that no real
/// part trips it, tight enough that a runaway stops in well under a second.
const MAX_PLANES: usize = 512;

/// Squared-magnitude floor below which a dual facet is treated as collapsed.
///
/// A guard against exact degeneracy (repeated or collinear dual points), not
/// against ill-conditioning. Polarity puts a dual point at `1/slack` from the
/// origin, so on a decimetre part the dual facets have edge lengths of order ten
/// and cross products of order a hundred: twenty-odd orders above this floor.
/// Nothing real is near it.
const DEGEN_EPS2: f64 = 1e-20;

/// Two cuts in one round must point at least this far apart, as a dot product of
/// unit normals, so 0.90 is about 26 degrees. It bounds how many planes a round
/// adds without letting one bulge be cut a dozen times over.
///
/// Swept over the eleven collision meshes at a 1 mm budget. Cutting one vertex
/// per round (0.999) costs 1097 fitted vertices; the value here gives 994; taking
/// every offender every round (0.0) reaches 973, but takes 43% longer to fit for
/// that last 2%. The curve is flat either side of this knee, so the exact value
/// is not delicate.
const BATCH_SEPARATION: f64 = 0.90;

/// How far outside the reference hull's own bounding radius a fitted vertex may
/// land before the halfspace intersection is treated as degenerate.
///
/// The bounding box seed confines every vertex to its own corners, at most
/// `sqrt(3)` radii out, and adding planes only shrinks the polytope, so real
/// geometry never passes 1.74 here. The margin to 8 is deliberate: a collapsed
/// dual facet throws its vertex orders of magnitude out, not just past the bound,
/// so a loose threshold separates the two cases cleanly while never firing on a
/// merely ill-conditioned one.
const EXTENT_LIMIT_RADII: f64 = 8.0;

/// A vertex of the working polytope that sits further outside the mesh hull than
/// the budget allows, with the direction whose supporting plane cuts it off.
struct Overshoot {
    deviation: f64,
    outward: Option<Unit<Vector3<f64>>>,
    vertex: Point3<f64>,
}

/// A mesh fitted by a circumscribing polytope.
pub struct Circumscribed {
    /// The polytope, as the hull of its own vertices. Contains the mesh.
    pub hull: ConvexHull,
    /// Supporting halfspaces the fit retained. Read by the tests that pin the
    /// cost of a budget; production consumes just the hull.
    #[cfg_attr(not(test), allow(dead_code))]
    pub planes: usize,
    /// The fit's slack at its loosest point: the largest distance (metres) from
    /// a vertex of the fit inward to the mesh's convex hull. The fit encloses
    /// the mesh, so this is how much *bigger* than the mesh it is, never a leak.
    /// Never exceeds the budget the fit was asked for, which is why production
    /// takes the guarantee and only the tests read the number.
    #[cfg_attr(not(test), allow(dead_code))]
    pub deviation: f64,
}

/// Fit `cloud` with a circumscribing polytope whose worst deviation from the
/// cloud's convex hull is at most `budget` metres. The result contains every
/// point of the cloud, so it never under-reports a distance to it.
///
/// Errors on a non-finite or degenerate cloud (fewer than four points, collinear,
/// coplanar), a non-positive budget, or a budget no plane count under
/// [`MAX_PLANES`] can meet.
pub fn circumscribe(cloud: &[Point3<f64>], budget: f64) -> Result<Circumscribed, String> {
    if !(budget.is_finite() && budget > 0.0) {
        return Err(format!(
            "deviation budget must be finite and positive, got {budget}"
        ));
    }
    if let Some(bad) = cloud
        .iter()
        .find(|p| !p.coords.iter().all(|x| x.is_finite()))
    {
        return Err(format!("mesh cloud holds a non-finite point {bad:?}"));
    }
    let reference = Reference::new(exact_hull(cloud)?)?;
    let center = reference.center();
    let extent_limit = EXTENT_LIMIT_RADII * reference.bound_radius();

    let mut planes: Vec<(Unit<Vector3<f64>>, f64)> = seed_cover()
        .map(|n| (n, support_offset(cloud, &n)))
        .collect();
    loop {
        let vertices = intersect(&planes, &center, extent_limit)?;
        if vertices.is_empty() {
            return Err("the halfspace intersection has no vertices".into());
        }
        let mut over: Vec<Overshoot> = vertices
            .iter()
            .map(|v| {
                let (deviation, outward) = reference.deviation(v);
                Overshoot {
                    deviation,
                    outward,
                    vertex: *v,
                }
            })
            .filter(|o| o.deviation > budget)
            .collect();
        if over.is_empty() {
            // Re-hulling the vertex set collapses the duplicates that coplanar
            // dual facets produce for a single primal corner.
            let hull = exact_hull(&vertices)?;
            let deviation = hull
                .vertices
                .iter()
                .map(|v| reference.deviation(v).0)
                .fold(0.0, f64::max);
            return Ok(Circumscribed {
                hull,
                planes: planes.len(),
                deviation,
            });
        }
        // Cut the worst offenders first, and as many per round as point in
        // meaningfully different directions. Batching is free in the result: a
        // plane that a later cut makes redundant has its dual point swallowed by
        // the dual hull, so it contributes no vertex and costs nothing at query
        // time. It is not free in build time, which is why the round is capped by
        // direction rather than taking every offender.
        over.sort_by(|x, y| y.deviation.total_cmp(&x.deviation));
        let mut round: Vec<Unit<Vector3<f64>>> = Vec::new();
        for o in over {
            let Some(outward) = o.outward else {
                return Err(format!(
                    "vertex {:?} is {} m outside the mesh hull with no separating direction",
                    o.vertex, o.deviation
                ));
            };
            if round.iter().any(|n| outward.dot(n) >= BATCH_SEPARATION) {
                continue;
            }
            round.push(outward);
            planes.push((outward, support_offset(cloud, &outward)));
        }
        if planes.len() > MAX_PLANES {
            return Err(format!(
                "{} supporting planes still leave a deviation over the {budget:.6} m budget",
                planes.len()
            ));
        }
    }
}

/// The cloud's support value in direction `n`: the plane `n . x = offset` touches
/// the cloud and holds all of it. Taken over the raw cloud rather than over any
/// fitted hull, so containment does not depend on the quality of an intermediate
/// fit.
fn support_offset(cloud: &[Point3<f64>], n: &Unit<Vector3<f64>>) -> f64 {
    cloud
        .iter()
        .map(|p| n.dot(&p.coords))
        .fold(f64::NEG_INFINITY, f64::max)
}

/// The six axis directions. Their supporting planes are the cloud's
/// axis-aligned bounding box, which is bounded whatever the cloud, and that is
/// what lets the refinement below assume a bounded polytope.
///
/// Chamfering the box first (adding the twelve edge and eight corner normals, so
/// 26 seeds) was measured to be worse on every axis: 1104 fitted vertices against
/// 1009, a slightly looser worst deviation, and a slower fit. Refinement puts
/// planes where the surface actually curves, so a chamfer seeded off the bounding
/// box is mostly redundant by the time the fit converges, having biased the early
/// rounds on its way there.
fn seed_cover() -> impl Iterator<Item = Unit<Vector3<f64>>> {
    [Vector3::x_axis(), Vector3::y_axis(), Vector3::z_axis()]
        .into_iter()
        .flat_map(|axis| [axis, -axis])
}

/// Vertices of the intersection of `planes`, by polarity about `center`.
///
/// With the origin at `center` (strictly inside every halfspace), a halfspace
/// `n . x <= h` maps to the dual point `n / (h - n . center)`, and the polytope's
/// vertices are the supporting planes of the dual points' convex hull: a dual
/// facet with plane `m . x = t` is the primal vertex `center + m / t`. So one
/// convex hull over as many points as there are planes enumerates every vertex,
/// with no triple-of-planes search.
fn intersect(
    planes: &[(Unit<Vector3<f64>>, f64)],
    center: &Point3<f64>,
    extent_limit: f64,
) -> Result<Vec<Point3<f64>>, String> {
    let mut dual = Vec::with_capacity(planes.len());
    for (n, h) in planes {
        let slack = h - n.dot(&center.coords);
        if slack <= 0.0 {
            return Err(format!(
                "the fit centre lies on or outside a supporting plane (slack {slack})"
            ));
        }
        dual.push(Point3::from(n.into_inner() / slack));
    }
    let dual = exact_hull(&dual)?;

    let mut vertices = Vec::with_capacity(dual.faces.len());
    for f in &dual.faces {
        let (a, b, c) = (
            dual.vertices[f[0]],
            dual.vertices[f[1]],
            dual.vertices[f[2]],
        );
        let m = (b - a).cross(&(c - a));
        if m.norm_squared() <= DEGEN_EPS2 {
            continue;
        }
        let m = m.normalize();
        // The dual hull encloses the origin, so one of the two orientations has a
        // positive plane offset; that is the facet's distance from the origin.
        let (m, t) = if m.dot(&a.coords) < 0.0 {
            (-m, -m.dot(&a.coords))
        } else {
            (m, m.dot(&a.coords))
        };
        if t <= 0.0 {
            return Err("a dual facet passes through the fit centre".into());
        }
        let vertex = Point3::from(center.coords + m / t);
        let reach = (vertex - *center).norm();
        if reach > extent_limit {
            return Err(format!(
                "the halfspace intersection is degenerate: a vertex sits {reach:.3} m \
                 from the fit centre, past the {extent_limit:.3} m limit"
            ));
        }
        vertices.push(vertex);
    }
    Ok(vertices)
}

/// The mesh's exact convex hull, set up to answer "how far outside is this
/// point, and in which direction" cheaply. Support is a hill-climb over the
/// vertex adjacency instead of a scan: a linear function on a convex polytope has
/// no local maximum that is not global, so climbing to a locally best vertex is
/// exact, and warm-starting from the previous answer keeps consecutive queries
/// (which the refinement issues in nearby directions) to a few steps.
struct Reference {
    vertices: Vec<Point3<f64>>,
    /// Neighbouring vertex indices per vertex, from the hull's triangles.
    adjacency: Vec<Vec<u32>>,
    center: Point3<f64>,
    radius: f64,
    /// Where the last support query landed, the next one's starting guess.
    warm: Cell<usize>,
}

impl Reference {
    fn new(hull: ConvexHull) -> Result<Reference, String> {
        if hull.vertices.is_empty() {
            return Err("the reference hull has no vertices".into());
        }
        let mut adjacency: Vec<Vec<u32>> = vec![Vec::new(); hull.vertices.len()];
        for f in &hull.faces {
            for (a, b) in [(f[0], f[1]), (f[1], f[2]), (f[2], f[0])] {
                adjacency[a].push(b as u32);
                adjacency[b].push(a as u32);
            }
        }
        for neighbours in &mut adjacency {
            neighbours.sort_unstable();
            neighbours.dedup();
        }
        if let Some(orphan) = adjacency.iter().position(Vec::is_empty) {
            return Err(format!(
                "reference hull vertex {orphan} borders no face, so a support \
                 hill-climb cannot leave it"
            ));
        }
        let center = Point3::from(
            hull.vertices
                .iter()
                .fold(Vector3::zeros(), |acc, v| acc + v.coords)
                / hull.vertices.len() as f64,
        );
        let radius = hull
            .vertices
            .iter()
            .map(|v| (v - center).norm())
            .fold(0.0, f64::max);
        if !(radius.is_finite() && radius > 0.0) {
            return Err(format!(
                "the reference hull has no extent (radius {radius})"
            ));
        }
        Ok(Reference {
            vertices: hull.vertices,
            adjacency,
            center,
            radius,
            warm: Cell::new(0),
        })
    }

    /// Centroid of the hull vertices: strictly inside the hull, so it is a valid
    /// polarity origin for any polytope that contains the hull.
    fn center(&self) -> Point3<f64> {
        self.center
    }

    /// Farthest hull vertex from [`center`](Self::center). Named apart from the
    /// [`Support::radius`] rounding this type deliberately does not have, so the
    /// two cannot be confused at a call site.
    fn bound_radius(&self) -> f64 {
        self.radius
    }

    /// How far `p` lies outside the hull: the true distance from `p` inward to
    /// the hull surface (zero when `p` is already inside), and the outward unit
    /// direction from the hull's nearest point to `p`. That direction's
    /// supporting plane touches the hull exactly at the nearest point, so
    /// cutting with it removes exactly this much slack.
    fn deviation(&self, p: &Point3<f64>) -> (f64, Option<Unit<Vector3<f64>>>) {
        let r = gjk::distance(&Vertex(*p), self);
        (r.distance.max(0.0), r.normal)
    }
}

impl Support for Reference {
    fn core_support(&self, dir: &Vector3<f64>) -> Point3<f64> {
        let mut best = self.warm.get();
        let mut best_dot = self.vertices[best].coords.dot(dir);
        // Each step strictly increases the objective, so the walk cannot cycle.
        loop {
            let step = self.adjacency[best]
                .iter()
                .map(|&n| (n as usize, self.vertices[n as usize].coords.dot(dir)))
                .filter(|&(_, d)| d > best_dot)
                .max_by(|x, y| x.1.total_cmp(&y.1));
            match step {
                Some((n, d)) => (best, best_dot) = (n, d),
                None => break,
            }
        }
        self.warm.set(best);
        self.vertices[best]
    }
}

/// A single point as a GJK body, for point-to-hull distance without allocating a
/// degenerate hull per query.
struct Vertex(Point3<f64>);

impl Support for Vertex {
    fn core_support(&self, _dir: &Vector3<f64>) -> Point3<f64> {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{RngExt, SeedableRng};

    fn pt(x: f64, y: f64, z: f64) -> Point3<f64> {
        Point3::new(x, y, z)
    }

    /// Triangle soup of an axis-aligned box, as the fit sees a mesh.
    fn box_cloud(half: [f64; 3]) -> Vec<Point3<f64>> {
        let mut out = Vec::new();
        for sx in [-1.0, 1.0] {
            for sy in [-1.0, 1.0] {
                for sz in [-1.0, 1.0] {
                    out.push(pt(sx * half[0], sy * half[1], sz * half[2]));
                }
            }
        }
        out
    }

    /// A dense fibonacci sphere of radius `r`, the worst case for a face budget.
    fn sphere_cloud(r: f64, n: usize) -> Vec<Point3<f64>> {
        (0..n)
            .map(|i| {
                let phi = i as f64 * 0.618 * std::f64::consts::TAU;
                let z = -1.0 + 2.0 * (i as f64 + 0.5) / n as f64;
                let rho = (1.0f64 - z * z).sqrt();
                pt(r * rho * phi.cos(), r * rho * phi.sin(), r * z)
            })
            .collect()
    }

    /// Worst distance from `cloud` out past the hull's faces: positive means the
    /// fit fails to contain the mesh, which is the one thing it must never do.
    fn worst_escape(hull: &ConvexHull, cloud: &[Point3<f64>]) -> f64 {
        let planes = crate::hull::face_planes(hull);
        cloud
            .iter()
            .map(|p| {
                planes
                    .iter()
                    .map(|(n, off)| n.dot(&p.coords) - off)
                    .fold(f64::NEG_INFINITY, f64::max)
            })
            .fold(f64::NEG_INFINITY, f64::max)
    }

    #[test]
    fn rejects_a_bad_budget_or_cloud() {
        let cloud = box_cloud([0.1, 0.1, 0.1]);
        assert!(circumscribe(&cloud, 0.0).is_err());
        assert!(circumscribe(&cloud, -0.001).is_err());
        assert!(circumscribe(&cloud, f64::NAN).is_err());
        assert!(circumscribe(&cloud, f64::INFINITY).is_err());
        let mut nan = cloud.clone();
        nan.push(pt(f64::NAN, 0.0, 0.0));
        assert!(
            circumscribe(&nan, 0.001).is_err(),
            "a non-finite mesh point must be refused, not encoded"
        );
    }

    /// A box is exactly representable: the fit should land on the eight corners
    /// with zero deviation, and no chamfer planes should survive.
    #[test]
    fn a_box_fits_exactly_with_eight_vertices() {
        let cloud = box_cloud([0.08, 0.05, 0.12]);
        let fit = circumscribe(&cloud, 0.001).expect("box fit");
        assert_eq!(fit.hull.vertices.len(), 8, "a box has eight vertices");
        assert!(
            fit.deviation < 1e-9,
            "a box is exactly representable, got {} m",
            fit.deviation
        );
        assert!(worst_escape(&fit.hull, &cloud) <= 1e-9);
    }

    /// The whole point of the budget: the measured worst deviation honours it,
    /// and tightening it costs vertices monotonically.
    #[test]
    fn a_tighter_budget_buys_accuracy_with_vertices() {
        let cloud = sphere_cloud(0.05, 4000);
        let mut previous = 0;
        for budget in [0.004, 0.002, 0.001] {
            let fit = circumscribe(&cloud, budget).expect("sphere fit");
            assert!(
                fit.deviation <= budget,
                "deviation {} exceeds the {budget} m budget",
                fit.deviation
            );
            assert!(
                worst_escape(&fit.hull, &cloud) <= 1e-9,
                "the fit must contain every mesh point"
            );
            assert!(
                fit.hull.vertices.len() > previous,
                "a tighter budget should need more vertices, got {} after {previous}",
                fit.hull.vertices.len()
            );
            previous = fit.hull.vertices.len();
        }
    }

    /// A long flat-sided body costs almost nothing: faces go where the surface
    /// curves, so a plate is not charged for its area. This is what vertex
    /// clustering gets backwards.
    #[test]
    fn a_flat_body_costs_no_more_than_a_small_one() {
        let small = circumscribe(&box_cloud([0.02, 0.02, 0.02]), 0.001).expect("small");
        let plate = circumscribe(&box_cloud([0.4, 0.3, 0.01]), 0.001).expect("plate");
        assert_eq!(
            small.hull.vertices.len(),
            plate.hull.vertices.len(),
            "8 each"
        );
    }

    /// Containment holds on an irregular cloud, and the fit compresses it.
    #[test]
    fn an_irregular_cloud_is_contained_and_compressed() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(17);
        let cloud: Vec<Point3<f64>> = (0..5000)
            .map(|_| {
                let v = Vector3::new(
                    rng.random_range(-1.0..1.0),
                    rng.random_range(-1.0..1.0),
                    rng.random_range(-1.0..1.0),
                );
                // Squash to an oblate blob so the surface has both flats and curves.
                Point3::from(Vector3::new(0.09 * v.x, 0.06 * v.y, 0.02 * v.z))
            })
            .collect();
        let fit = circumscribe(&cloud, 0.001).expect("blob fit");
        assert!(worst_escape(&fit.hull, &cloud) <= 1e-9);
        assert!(fit.deviation <= 0.001);
        assert!(
            fit.hull.vertices.len() < 200,
            "expected a compact fit, got {} vertices",
            fit.hull.vertices.len()
        );
    }

    /// The hill-climbing support must agree with an exhaustive scan in every
    /// direction, or the deviation it feeds is understated.
    #[test]
    fn hill_climbing_support_matches_an_exhaustive_scan() {
        let raw = crate::stl::load_stl(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/meshes/link6_symp.stl"
        ))
        .expect("mesh");
        let cloud: Vec<Point3<f64>> = raw
            .iter()
            .map(|v| pt(v.x * 0.001, v.y * 0.001, v.z * 0.001))
            .collect();
        let reference = Reference::new(exact_hull(&cloud).expect("hull")).expect("reference");
        let mut rng = rand::rngs::StdRng::seed_from_u64(23);
        for _ in 0..2000 {
            let dir = Vector3::new(
                rng.random_range(-1.0..1.0),
                rng.random_range(-1.0..1.0),
                rng.random_range(-1.0..1.0),
            );
            if dir.norm_squared() < 1e-12 {
                continue;
            }
            let climbed = reference.core_support(&dir).coords.dot(&dir);
            let scanned = reference
                .vertices
                .iter()
                .map(|v| v.coords.dot(&dir))
                .fold(f64::NEG_INFINITY, f64::max);
            assert!(
                (climbed - scanned).abs() < 1e-12,
                "hill-climb {climbed} missed the true support {scanned}"
            );
        }
    }

    /// Deviation is true Euclidean distance, not a face-plane violation. A point
    /// off a right-angle corner violates each face by 1/sqrt(2) of its real
    /// distance, so the two metrics must visibly disagree there and the fit must
    /// report the larger one.
    #[test]
    fn deviation_is_true_distance_not_plane_violation() {
        let cloud = box_cloud([0.1, 0.1, 0.1]);
        let hull = exact_hull(&cloud).expect("hull");
        let reference = Reference::new(hull.clone()).expect("reference");
        let corner = pt(0.103, 0.103, 0.103);
        let (true_distance, _) = reference.deviation(&corner);
        let violation = crate::hull::face_planes(&hull)
            .iter()
            .map(|(n, off)| n.dot(&corner.coords) - off)
            .fold(f64::NEG_INFINITY, f64::max);
        assert!(
            (true_distance - 0.003 * 3.0f64.sqrt()).abs() < 1e-9,
            "corner distance should be the space diagonal, got {true_distance}"
        );
        assert!(
            violation < true_distance * 0.7,
            "the plane metric should understate: {violation} vs {true_distance}"
        );
    }

    /// A real collision mesh: contained, inside budget, and a real compression of
    /// the cloud. Not asserted against the exact hull's vertex count: on a small
    /// part, whose hull is already only ~100 vertices, holding a millimetre costs
    /// about the same. The compression is on the large meshes, where an exact hull
    /// runs to thousands.
    #[test]
    fn a_real_collision_mesh_fits_inside_budget() {
        let raw = crate::stl::load_stl(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/meshes/link6_symp.stl"
        ))
        .expect("mesh");
        let cloud: Vec<Point3<f64>> = raw
            .iter()
            .map(|v| pt(v.x * 0.001, v.y * 0.001, v.z * 0.001))
            .collect();
        let fit = circumscribe(&cloud, 0.001).expect("fit");
        assert!(
            worst_escape(&fit.hull, &cloud) <= 1e-9,
            "the fit must contain every mesh point"
        );
        assert!(fit.deviation <= 0.001, "deviation {}", fit.deviation);
        assert!(
            fit.hull.vertices.len() * 8 < cloud.len(),
            "expected a compact fit, got {} vertices from {} points",
            fit.hull.vertices.len(),
            cloud.len()
        );
    }

    /// What each deviation budget costs in planes, vertices, and fit time on a
    /// real collision mesh. Run deliberately when picking the operating point:
    ///
    /// ```sh
    /// cargo test --release budget_sweep_report -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "measurement probe, not a pass/fail check"]
    fn budget_sweep_report() {
        let raw = crate::stl::load_stl(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/meshes/link6_symp.stl"
        ))
        .expect("mesh");
        let cloud: Vec<Point3<f64>> = raw
            .iter()
            .map(|v| pt(v.x * 0.001, v.y * 0.001, v.z * 0.001))
            .collect();
        let start = std::time::Instant::now();
        let exact = exact_hull(&cloud).expect("exact hull");
        println!(
            "\nmesh {} points -> exact hull {} vertices, {} faces in {:.0} ms",
            cloud.len(),
            exact.vertices.len(),
            exact.faces.len(),
            start.elapsed().as_secs_f64() * 1e3
        );
        println!("\nbudget (mm)  planes  vertices  deviation (mm)  fit (ms)");
        for budget in [8.0, 4.0, 2.0, 1.0, 0.5, 0.25] {
            let start = std::time::Instant::now();
            let fit = circumscribe(&cloud, budget * 1e-3).expect("fit");
            println!(
                "{budget:11.2}  {:6}  {:8}  {:14.4}  {:8.1}",
                fit.planes,
                fit.hull.vertices.len(),
                fit.deviation * 1e3,
                start.elapsed().as_secs_f64() * 1e3
            );
        }
        println!();
    }

    /// The fit does not depend on the caller's point order.
    #[test]
    fn the_fit_is_order_independent() {
        let cloud = sphere_cloud(0.04, 1500);
        let a = circumscribe(&cloud, 0.001).expect("fit");
        let mut reversed = cloud.clone();
        reversed.reverse();
        let b = circumscribe(&reversed, 0.001).expect("fit");
        assert_eq!(a.hull.vertices, b.hull.vertices);
        assert_eq!(a.planes, b.planes);
    }
}
