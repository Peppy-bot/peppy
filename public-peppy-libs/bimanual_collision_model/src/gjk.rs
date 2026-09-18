//! Gilbert-Johnson-Keerthi distance between convex bodies.
//!
//! GJK computes the distance between two convex shapes from nothing but their
//! support functions (the farthest point in a direction). It works on the
//! Minkowski difference `A - B`: the shapes are disjoint exactly when the
//! origin lies outside it, and their separation is the distance from the origin
//! to it. The algorithm walks a simplex (1 to 4 points of the difference)
//! toward the origin, each step adding the support point in the direction of
//! the origin and dropping the vertices that no longer carry the closest point,
//! until no support point lies closer (Gilbert, Johnson & Keerthi 1988; van den
//! Bergen, "Collision Detection in Interactive 3D Environments"; the simplex
//! sub-distance follows Ericson, "Real-Time Collision Detection", ch. 5).
//!
//! Distance is computed on the unrounded cores, then the rounding radii are
//! subtracted (the standard margin trick), so a hull's inflation radius stays
//! separable from the query. When the cores overlap, EPA (below) recovers the
//! penetration depth and direction, so the signed distance is continuous
//! through contact: separating motion is always distinguishable from
//! approaching, even from inside an overlap.

use std::collections::{HashMap, HashSet};

use srs_model::nalgebra::{Isometry3, Point3, Unit, Vector3};

use crate::hull::ConvexHull;

/// Relative duality-gap tolerance: stop when the support point beyond the
/// current closest point `v` improves the origin distance by less than this
/// fraction of `v . v`. At 1e-10 the distance is tight to roughly float
/// precision without spinning on shapes whose surfaces are nearly flat.
const REL_TOL: f64 = 1e-10;

/// Squared length below which a vector is treated as the origin: cores touch
/// (1e-10 m)^2, the same surface round-off scale the geometry module uses.
const ORIGIN_EPS2: f64 = 1e-20;

/// Iteration cap. GJK on simple primitives converges in well under ten steps;
/// this only backstops a numerical stall, never a normal query.
const MAX_ITERS: u32 = 32;

/// EPA convergence: stop expanding once a new support point lies within this of
/// the current closest face of the Minkowski difference (metres).
const EPA_TOL: f64 = 1e-9;

/// EPA expansion cap. Penetration on convex pieces resolves in a few dozen
/// faces; this backstops a numerical stall.
const EPA_MAX_ITERS: u32 = 64;

/// Relative degeneracy floor for simplex features: a tetrahedron whose volume
/// is below this fraction of its edge-scale cubed (or a triangle whose area is
/// below this fraction of its edge-scale squared) is treated as flat. A flat
/// feature has no interior, so it can neither enclose the origin nor support
/// the interior-region arithmetic; the sub-distance falls back to its boundary
/// features instead.
const SIMPLEX_DEGEN_REL: f64 = 1e-9;

/// A convex body usable by [`distance`]: its core support function (the
/// farthest core point along a direction) and an optional rounding radius
/// swept around that core.
pub trait Support {
    /// Farthest point of the unrounded core in direction `dir` (need not be
    /// normalized).
    fn core_support(&self, dir: &Vector3<f64>) -> Point3<f64>;

    /// Radius swept around the core (zero for a polytope).
    fn radius(&self) -> f64 {
        0.0
    }
}

/// A convex polytope GJK primitive: hull vertices and an optional rounding
/// `radius`. Support is an exact scan over the vertices: fitted hulls carry
/// tens to a few hundred vertices, where a branch-predictable dot scan is
/// fast, its argmax is exact for any vertex set regardless of face-graph
/// quality (repair and clipping can leave the triangle graph locally
/// non-convex), and it is stateless, so a query's answer cannot depend on the
/// queries before it.
#[derive(Debug)]
pub struct Hull {
    vertices: Vec<Point3<f64>>,
    faces: Vec<[usize; 3]>,
    radius: f64,
    /// Bounding sphere of the rounded piece in its own frame (vertex centroid;
    /// farthest vertex plus the rounding radius). Placing just the centre gives
    /// a cheap lower bound on any pairwise distance, so a multi-piece narrowphase
    /// can skip piece pairs that cannot beat the best distance found.
    bound_center: Point3<f64>,
    bound_radius: f64,
}

/// Result of a GJK query: signed surface distance (negative is penetration of
/// the rounded shapes) and the closest point on each surface.
#[derive(Debug, Clone)]
pub struct GjkDistance {
    pub distance: f64,
    pub on_a: Point3<f64>,
    pub on_b: Point3<f64>,
    /// Unit direction along which translating body `a` increases the signed
    /// distance (`b` decreases it). Carried explicitly: the witness difference
    /// `on_b - on_a` reverses sense with the sign of the distance, so it cannot
    /// serve as a gradient direction. `None` only for degenerate core contact
    /// (the surfaces then overlap by the summed radii), where no direction is
    /// defined.
    pub normal: Option<Unit<Vector3<f64>>>,
    /// GJK iterations the query took to converge. Read only by the tests that
    /// pin convergence behavior; production consumes just the geometry above.
    #[cfg_attr(not(test), allow(dead_code))]
    pub iterations: u32,
}

/// A vertex of the Minkowski difference, carrying the two core support points
/// it came from so the witnesses can be recovered by the same barycentric
/// weights that locate the closest point.
#[derive(Debug, Clone, Copy)]
struct SupportPoint {
    v: Vector3<f64>,
    a: Point3<f64>,
    b: Point3<f64>,
}

/// The Minkowski-difference support point in direction `dir`: the farthest core
/// point of `a` minus the farthest of `b` in the opposite direction, carrying
/// both witnesses so they can be blended back later.
fn support(a: &impl Support, b: &impl Support, dir: &Vector3<f64>) -> SupportPoint {
    let pa = a.core_support(dir);
    let pb = b.core_support(&(-dir));
    SupportPoint {
        v: pa.coords - pb.coords,
        a: pa,
        b: pb,
    }
}

/// The point of a (sub-)simplex closest to the origin, reduced to the vertices
/// that actually carry it: `closest` as a vector from the origin, the carrying
/// `simplex`, and the barycentric `weights` that locate it. Each GJK step
/// reduces the working simplex to one of these; the weights later blend the two
/// surface witnesses.
struct SubSimplex {
    closest: Vector3<f64>,
    simplex: Vec<SupportPoint>,
    weights: Vec<f64>,
}

impl SubSimplex {
    /// A lone carrying vertex (the closest feature is a corner), weight one.
    fn vertex(p: SupportPoint) -> SubSimplex {
        SubSimplex {
            closest: p.v,
            simplex: vec![p],
            weights: vec![1.0],
        }
    }

    /// A carrying edge `[a, b]` with the origin projecting at parameter `t`.
    fn edge(closest: Vector3<f64>, a: SupportPoint, b: SupportPoint, t: f64) -> SubSimplex {
        SubSimplex {
            closest,
            simplex: vec![a, b],
            weights: vec![1.0 - t, t],
        }
    }
}

/// Centroid-anchored enclosing sphere of one or more rounded vertex clouds:
/// the centre is the mean of all core vertices, the radius the farthest vertex
/// plus its cloud's rounding radius. Single-sourced so the whole-body pair
/// broadphase and the per-piece prefilter compute the same bound and cannot
/// drift apart.
pub(crate) fn enclosing_sphere<'a, I>(clouds: I) -> (Point3<f64>, f64)
where
    I: Iterator<Item = (&'a [Point3<f64>], f64)> + Clone,
{
    let (sum, count) = clouds
        .clone()
        .flat_map(|(points, _)| points)
        .fold((Vector3::zeros(), 0_usize), |(sum, count), p| {
            (sum + p.coords, count + 1)
        });
    let center = Point3::from(sum / count.max(1) as f64);
    let radius = clouds
        .flat_map(|(points, rounding)| points.iter().map(move |p| (p - center).norm() + rounding))
        .fold(0.0_f64, f64::max);
    (center, radius)
}

impl Hull {
    /// A GJK primitive from a computed [`ConvexHull`]. Errors on an empty hull
    /// or an invalid rounding radius: the radius feeds both the query and the
    /// bounding-sphere prefilter, where a negative or non-finite value would
    /// make the pruning bound unsound and silently skip the true nearest piece.
    pub fn new(hull: &ConvexHull, radius: f64) -> Result<Hull, String> {
        if hull.vertices.is_empty() {
            return Err("cannot build a hull from zero vertices".into());
        }
        if !radius.is_finite() || radius < 0.0 {
            return Err(format!(
                "hull rounding radius must be finite and non-negative, got {radius}"
            ));
        }
        let (bound_center, bound_radius) =
            enclosing_sphere(std::iter::once((hull.vertices.as_slice(), radius)));
        Ok(Hull {
            vertices: hull.vertices.clone(),
            faces: hull.faces.clone(),
            radius,
            bound_center,
            bound_radius,
        })
    }

    /// Centre of the piece's bounding sphere, in the hull's own frame.
    pub fn bound_center(&self) -> Point3<f64> {
        self.bound_center
    }

    /// Radius of the piece's bounding sphere (rounding included).
    pub fn bound_radius(&self) -> f64 {
        self.bound_radius
    }

    /// The hull vertices, for placement and rendering.
    pub fn vertices(&self) -> &[Point3<f64>] {
        &self.vertices
    }

    /// The hull faces (triangles indexing into [`vertices`](Self::vertices)),
    /// for rendering.
    pub fn faces(&self) -> &[[usize; 3]] {
        &self.faces
    }

    /// The rounding radius swept around the core (the simplification inflation).
    pub fn inflation(&self) -> f64 {
        self.radius
    }
}

impl Support for Hull {
    fn core_support(&self, dir: &Vector3<f64>) -> Point3<f64> {
        // Exact argmax over the vertices (see the struct doc). A plain
        // comparison loop keeps the scan branch-predictable and NaN-free (a
        // finite dir dotted with finite vertices), so it optimizes tightly.
        let mut best = 0;
        let mut best_dot = f64::NEG_INFINITY;
        for (i, v) in self.vertices.iter().enumerate() {
            let dot = v.coords.dot(dir);
            if dot > best_dot {
                (best, best_dot) = (i, dot);
            }
        }
        self.vertices[best]
    }

    fn radius(&self) -> f64 {
        self.radius
    }
}

/// A hull placed by an isometry. Support rotates the query direction into the
/// hull's own frame instead of transforming vertices, so it stays O(1).
pub struct Placed<'a> {
    hull: &'a Hull,
    iso: Isometry3<f64>,
}

impl<'a> Placed<'a> {
    pub fn new(hull: &'a Hull, iso: Isometry3<f64>) -> Placed<'a> {
        Placed { hull, iso }
    }
}

impl Support for Placed<'_> {
    fn core_support(&self, dir: &Vector3<f64>) -> Point3<f64> {
        self.iso
            * self
                .hull
                .core_support(&self.iso.inverse_transform_vector(dir))
    }

    fn radius(&self) -> f64 {
        self.hull.radius()
    }
}

/// Signed surface distance between two convex bodies and the closest points.
/// Runs GJK on the cores, then subtracts the radii and pushes the witnesses out
/// to the rounded surfaces along the separating direction.
pub fn distance(a: &impl Support, b: &impl Support) -> GjkDistance {
    let (ra, rb) = (a.radius(), b.radius());

    let mut simplex = vec![support(a, b, &Vector3::x())];
    let mut weights = vec![1.0];
    let mut v = simplex[0].v;
    let mut iterations = 0;

    while v.norm_squared() > ORIGIN_EPS2 {
        let w = support(a, b, &(-v));
        let vv = v.norm_squared();
        // Duality gap: ||v|| - (v . w)/||v|| as a fraction of ||v||. Once the
        // farthest point toward the origin is no closer than v's plane, v is
        // the answer.
        if vv - v.dot(&w.v) <= REL_TOL * vv {
            break;
        }
        // A repeated support direction means no new vertex is reachable.
        if simplex
            .iter()
            .any(|s| (s.v - w.v).norm_squared() <= ORIGIN_EPS2)
        {
            break;
        }
        simplex.push(w);
        let reduced = closest_to_origin(&simplex);
        // Origin reached: the cores overlap (or just touch). Hand the carrying
        // simplex to EPA for the penetration depth and direction.
        if reduced.closest.norm_squared() <= ORIGIN_EPS2 {
            return epa(a, b, &reduced.simplex, ra, rb);
        }
        v = reduced.closest;
        simplex = reduced.simplex;
        weights = reduced.weights;
        iterations += 1;
        if iterations >= MAX_ITERS {
            break;
        }
    }

    let core_a = weighted_point(&simplex, &weights, |s| s.a);
    let core_b = weighted_point(&simplex, &weights, |s| s.b);
    let core_dist = v.norm();
    // `v` points from b's core witness toward a's: the separating direction,
    // whether the rounded surfaces are clear or in shallow overlap.
    let (on_a, on_b, normal) = if core_dist > ORIGIN_EPS2.sqrt() {
        let n = v / core_dist;
        (
            core_a - n * ra,
            core_b + n * rb,
            Some(Unit::new_unchecked(n)),
        )
    } else {
        (core_a, core_b, None)
    };
    GjkDistance {
        distance: core_dist - ra - rb,
        on_a,
        on_b,
        normal,
        iterations,
    }
}

/// A face of the EPA polytope: vertex indices into the growing point list, its
/// outward unit normal (away from the enclosed origin), and the origin's
/// distance to its plane.
#[derive(Clone, Copy)]
struct EpaFace {
    v: [usize; 3],
    normal: Vector3<f64>,
    dist: f64,
}

/// Expanding Polytope Algorithm: the GJK simplex `kept` carries the origin
/// (cores overlap), so grow a polytope around it toward the nearest face of the
/// Minkowski difference, which gives the penetration depth and direction.
/// Returns the signed (negative) surface distance and the contact witnesses;
/// the same margin trick as the separated case carries the radii. This reuses
/// the horizon-stitching of the convex hull: a new support point deletes the
/// faces it can see and is joined to the horizon they leave behind.
fn epa(a: &impl Support, b: &impl Support, kept: &[SupportPoint], ra: f64, rb: f64) -> GjkDistance {
    // A closed polytope strictly enclosing the origin. GJK stops on a
    // tetrahedron for a clear overlap, or on a triangle with the origin in its
    // plane for a shallow one; lift that triangle to a bipyramid with a support
    // point on each side. Anything lower is a true touch, depth zero.
    let (mut points, mut faces): (Vec<SupportPoint>, Vec<EpaFace>) = match kept.len() {
        4 => (
            kept.to_vec(),
            [[0, 1, 2], [0, 1, 3], [0, 2, 3], [1, 2, 3]]
                .into_iter()
                .filter_map(|v| epa_face(v, kept))
                .collect(),
        ),
        3 => {
            let n = (kept[1].v - kept[0].v).cross(&(kept[2].v - kept[0].v));
            if n.norm_squared() <= ORIGIN_EPS2 {
                return touch(kept, ra, rb);
            }
            let n = n.normalize();
            let (wp, wm) = (support(a, b, &n), support(a, b, &(-n)));
            if wp.v.dot(&n) <= EPA_TOL || wm.v.dot(&n) >= -EPA_TOL {
                return touch(kept, ra, rb);
            }
            let points = vec![kept[0], kept[1], kept[2], wp, wm];
            let faces = [
                [0, 1, 3],
                [1, 2, 3],
                [2, 0, 3],
                [0, 1, 4],
                [1, 2, 4],
                [2, 0, 4],
            ]
            .into_iter()
            .filter_map(|v| epa_face(v, &points))
            .collect();
            (points, faces)
        }
        _ => return touch(kept, ra, rb),
    };

    // Expand toward the boundary, keeping the closest face seen as `best` so a
    // degenerate step (empty horizon, polytope opening up) returns a sound
    // answer instead of panicking.
    let mut iterations = 0;
    let mut best: Option<EpaFace> = None;
    while let Some(closest) =
        (0..faces.len()).min_by(|&x, &y| faces[x].dist.total_cmp(&faces[y].dist))
    {
        let face = faces[closest];
        best = Some(face);
        let w = support(a, b, &face.normal);
        iterations += 1;
        // Converged (support no farther than the face), capped, or the support
        // is already a vertex (no progress).
        if w.v.dot(&face.normal) - face.dist < EPA_TOL
            || iterations >= EPA_MAX_ITERS
            || points
                .iter()
                .any(|p| (p.v - w.v).norm_squared() <= ORIGIN_EPS2)
        {
            break;
        }
        // Delete every face the new point can see, stitch it to the horizon.
        let visible: Vec<usize> = faces
            .iter()
            .enumerate()
            .filter(|(_, f)| w.v.dot(&f.normal) > f.dist)
            .map(|(i, _)| i)
            .collect();
        let horizon = epa_horizon(&faces, &visible);
        if horizon.is_empty() {
            break;
        }
        let dropped: HashSet<usize> = visible.into_iter().collect();
        faces = faces
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !dropped.contains(i))
            .map(|(_, f)| f)
            .collect();
        let wi = points.len();
        points.push(w);
        for (i, j) in horizon {
            if let Some(f) = epa_face([i, j, wi], &points) {
                faces.push(f);
            }
        }
    }
    match best {
        Some(face) => epa_result(&points, &face, ra, rb, iterations),
        None => touch(kept, ra, rb),
    }
}

/// Zero-depth contact: the cores touch, witnesses taken at the carrier point.
/// `kept` is the GJK carrier simplex, which always has at least one point.
fn touch(kept: &[SupportPoint], ra: f64, rb: f64) -> GjkDistance {
    debug_assert!(!kept.is_empty(), "the GJK carrier simplex is never empty");
    GjkDistance {
        distance: -ra - rb,
        on_a: kept[0].a,
        on_b: kept[0].b,
        normal: None,
        iterations: 0,
    }
}

/// A polytope face from three difference vertices, oriented so its normal
/// points away from the enclosed origin (nonnegative plane offset). `None` if
/// the three points are collinear.
fn epa_face(v: [usize; 3], points: &[SupportPoint]) -> Option<EpaFace> {
    let (pa, pb, pc) = (points[v[0]].v, points[v[1]].v, points[v[2]].v);
    let n = (pb - pa).cross(&(pc - pa));
    if n.norm_squared() <= ORIGIN_EPS2 {
        return None;
    }
    let normal = n.normalize();
    let dist = normal.dot(&pa);
    if dist < 0.0 {
        Some(EpaFace {
            v,
            normal: -normal,
            dist: -dist,
        })
    } else {
        Some(EpaFace { v, normal, dist })
    }
}

/// Undirected edges bordering exactly one visible face: the horizon the new
/// faces attach to.
fn epa_horizon(faces: &[EpaFace], visible: &[usize]) -> Vec<(usize, usize)> {
    let mut count: HashMap<(usize, usize), u32> = HashMap::new();
    for &fi in visible {
        let v = faces[fi].v;
        for (a, b) in [(v[0], v[1]), (v[1], v[2]), (v[2], v[0])] {
            *count
                .entry(if a < b { (a, b) } else { (b, a) })
                .or_insert(0) += 1;
        }
    }
    count
        .into_iter()
        .filter(|&(_, c)| c == 1)
        .map(|(e, _)| e)
        .collect()
}

/// Signed distance and witnesses from the closest face: depth `face.dist` along
/// `face.normal`, witnesses the barycentric blend of the face's support points.
fn epa_result(
    points: &[SupportPoint],
    face: &EpaFace,
    ra: f64,
    rb: f64,
    iterations: u32,
) -> GjkDistance {
    let (pa, pb, pc) = (points[face.v[0]], points[face.v[1]], points[face.v[2]]);
    let [l0, l1, l2] = barycentric(face.normal * face.dist, pa.v, pb.v, pc.v);
    let on_a = Point3::from(pa.a.coords * l0 + pb.a.coords * l1 + pc.a.coords * l2);
    let on_b = Point3::from(pa.b.coords * l0 + pb.b.coords * l1 + pc.b.coords * l2);
    // The face normal deepens the overlap for a; a separates along its negation.
    GjkDistance {
        distance: -face.dist - ra - rb,
        on_a: on_a - face.normal * ra,
        on_b: on_b + face.normal * rb,
        normal: Some(Unit::new_unchecked(-face.normal)),
        iterations,
    }
}

/// Barycentric coordinates of `p` in triangle `abc` (Ericson 3.4).
fn barycentric(p: Vector3<f64>, a: Vector3<f64>, b: Vector3<f64>, c: Vector3<f64>) -> [f64; 3] {
    let (e0, e1, e2) = (b - a, c - a, p - a);
    let (d00, d01, d11) = (e0.dot(&e0), e0.dot(&e1), e1.dot(&e1));
    let (d20, d21) = (e2.dot(&e0), e2.dot(&e1));
    let denom = d00 * d11 - d01 * d01;
    if denom.abs() < 1e-18 {
        return [1.0, 0.0, 0.0];
    }
    let v = (d11 * d20 - d01 * d21) / denom;
    let w = (d00 * d21 - d01 * d20) / denom;
    [1.0 - v - w, v, w]
}

/// Barycentric blend of a chosen core point over the simplex.
fn weighted_point(
    simplex: &[SupportPoint],
    weights: &[f64],
    pick: impl Fn(&SupportPoint) -> Point3<f64>,
) -> Point3<f64> {
    let blended = simplex
        .iter()
        .zip(weights)
        .fold(Vector3::zeros(), |acc, (s, &w)| acc + pick(s).coords * w);
    Point3::from(blended)
}

/// Closest point of the simplex's convex hull to the origin, reduced to the
/// sub-simplex that carries it with its barycentric weights.
fn closest_to_origin(simplex: &[SupportPoint]) -> SubSimplex {
    match simplex {
        [a] => SubSimplex::vertex(*a),
        [a, b] => closest_segment(*a, *b),
        [a, b, c] => closest_triangle(*a, *b, *c),
        [a, b, c, d] => closest_tetrahedron(*a, *b, *c, *d),
        _ => unreachable!("GJK simplex holds one to four points"),
    }
}

fn closest_segment(a: SupportPoint, b: SupportPoint) -> SubSimplex {
    let ab = b.v - a.v;
    let len2 = ab.norm_squared();
    if len2 <= ORIGIN_EPS2 {
        return SubSimplex::vertex(a);
    }
    let t = (-a.v.dot(&ab) / len2).clamp(0.0, 1.0);
    if t <= 0.0 {
        SubSimplex::vertex(a)
    } else if t >= 1.0 {
        SubSimplex::vertex(b)
    } else {
        SubSimplex::edge(a.v + ab * t, a, b, t)
    }
}

/// Closest point on triangle `abc` to the origin (Ericson 5.1.5, query point at
/// the origin), returned as the carrying sub-simplex and weights.
fn closest_triangle(a: SupportPoint, b: SupportPoint, c: SupportPoint) -> SubSimplex {
    let (pa, pb, pc) = (a.v, b.v, c.v);
    let ab = pb - pa;
    let ac = pc - pa;

    // A degenerate (collinear or duplicate-point) triangle has no interior and
    // its region tests below divide by a vanishing area: its closest feature is
    // the best of its edges, which are robust to collapsed points. Plateau ties
    // in the support scan feed such triangles in.
    let area2 = ab.cross(&ac).norm_squared();
    let scale2 = ab
        .norm_squared()
        .max(ac.norm_squared())
        .max((pc - pb).norm_squared());
    if area2 <= SIMPLEX_DEGEN_REL * SIMPLEX_DEGEN_REL * scale2 * scale2 {
        return [
            closest_segment(a, b),
            closest_segment(a, c),
            closest_segment(b, c),
        ]
        .into_iter()
        .min_by(|x, y| {
            x.closest
                .norm_squared()
                .total_cmp(&y.closest.norm_squared())
        })
        .expect("three edges");
    }

    // Vertex regions. `ap = origin - pa = -pa`, and so on.
    let d1 = ab.dot(&-pa);
    let d2 = ac.dot(&-pa);
    if d1 <= 0.0 && d2 <= 0.0 {
        return SubSimplex::vertex(a);
    }
    let d3 = ab.dot(&-pb);
    let d4 = ac.dot(&-pb);
    if d3 >= 0.0 && d4 <= d3 {
        return SubSimplex::vertex(b);
    }
    let d5 = ab.dot(&-pc);
    let d6 = ac.dot(&-pc);
    if d6 >= 0.0 && d5 <= d6 {
        return SubSimplex::vertex(c);
    }

    // Edge regions.
    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        let t = d1 / (d1 - d3);
        return SubSimplex::edge(pa + ab * t, a, b, t);
    }
    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        let t = d2 / (d2 - d6);
        return SubSimplex::edge(pa + ac * t, a, c, t);
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        let t = (d4 - d3) / ((d4 - d3) + (d5 - d6));
        return SubSimplex::edge(pb + (pc - pb) * t, b, c, t);
    }

    // Face interior.
    let denom = 1.0 / (va + vb + vc);
    let v = vb * denom;
    let w = vc * denom;
    SubSimplex {
        closest: pa + ab * v + ac * w,
        simplex: vec![a, b, c],
        weights: vec![1.0 - v - w, v, w],
    }
}

/// Closest point on tetrahedron `abcd` to the origin (Ericson 5.1.6): the
/// closest point over whichever faces the origin lies outside of, or the origin
/// itself (zero, overlap) when it lies inside all four.
fn closest_tetrahedron(
    a: SupportPoint,
    b: SupportPoint,
    c: SupportPoint,
    d: SupportPoint,
) -> SubSimplex {
    let faces = [(a, b, c, d), (a, c, d, b), (a, d, b, c), (b, d, c, a)];
    // A flat tetrahedron (near-coplanar support points, fed in by plateau ties
    // in the support scan) has no interior, so it cannot enclose the origin,
    // and its face-plane tests below compare noise against noise: "inside all
    // faces" would misread as enclosure and hand EPA a simplex that fabricates
    // a penetration for shapes that are far apart. Its closest feature is the
    // best of its faces.
    let (ab, ac, ad) = (b.v - a.v, c.v - a.v, d.v - a.v);
    let volume = ab.dot(&ac.cross(&ad));
    let scale = ab
        .norm_squared()
        .max(ac.norm_squared())
        .max(ad.norm_squared())
        .max((c.v - b.v).norm_squared())
        .max((d.v - b.v).norm_squared())
        .max((d.v - c.v).norm_squared())
        .sqrt();
    if volume.abs() <= SIMPLEX_DEGEN_REL * scale * scale * scale {
        return faces
            .into_iter()
            .map(|(p, q, r, _)| closest_triangle(p, q, r))
            .min_by(|x, y| {
                x.closest
                    .norm_squared()
                    .total_cmp(&y.closest.norm_squared())
            })
            .expect("four faces");
    }

    let mut best: Option<SubSimplex> = None;
    // Each face listed with the opposite vertex, which fixes the inward side.
    for (p, q, r, opp) in faces {
        if !origin_outside_plane(p.v, q.v, r.v, opp.v) {
            continue;
        }
        let face = closest_triangle(p, q, r);
        if best
            .as_ref()
            .is_none_or(|b| face.closest.norm_squared() < b.closest.norm_squared())
        {
            best = Some(face);
        }
    }
    // Inside all faces: the origin is enclosed, the cores overlap.
    best.unwrap_or(SubSimplex {
        closest: Vector3::zeros(),
        simplex: vec![a, b, c, d],
        weights: vec![0.25; 4],
    })
}

/// Whether the origin lies on the far side of plane `pqr` from `opp` (so the
/// face can carry the closest point). A degenerate face is never outside.
fn origin_outside_plane(
    p: Vector3<f64>,
    q: Vector3<f64>,
    r: Vector3<f64>,
    opp: Vector3<f64>,
) -> bool {
    let n = (q - p).cross(&(r - p));
    let origin_side = -p.dot(&n);
    let opp_side = (opp - p).dot(&n);
    origin_side * opp_side < 0.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{RngExt, SeedableRng};
    use srs_model::nalgebra::{Isometry3, Translation3, UnitQuaternion};

    fn pt(x: f64, y: f64, z: f64) -> Point3<f64> {
        Point3::new(x, y, z)
    }

    fn sp(x: f64, y: f64, z: f64) -> SupportPoint {
        SupportPoint {
            v: Vector3::new(x, y, z),
            a: pt(x, y, z),
            b: pt(0.0, 0.0, 0.0),
        }
    }

    #[test]
    fn rejects_an_invalid_rounding_radius() {
        // The radius feeds the prefilter's pruning bound; a negative or
        // non-finite value would make that bound unsound rather than erroring.
        let tri = ConvexHull {
            vertices: vec![
                pt(0.0, 0.0, 0.0),
                pt(1.0, 0.0, 0.0),
                pt(0.0, 1.0, 0.0),
                pt(0.0, 0.0, 1.0),
            ],
            faces: Vec::new(),
        };
        assert!(Hull::new(&tri, -0.001).is_err());
        assert!(Hull::new(&tri, f64::NAN).is_err());
        assert!(Hull::new(&tri, f64::INFINITY).is_err());
        assert!(Hull::new(&tri, 0.0).is_ok());
    }

    #[test]
    fn a_flat_tetrahedron_does_not_enclose_the_origin() {
        // Four near-coplanar support points a metre from the origin: with no
        // interior, "inside all four face planes" is vacuously true and the
        // enclosure fallback would hand EPA a garbage simplex claiming deep
        // penetration. The degeneracy guard must return the true face-feature
        // distance instead.
        let flat = closest_tetrahedron(
            sp(-0.1, -0.1, 1.0),
            sp(0.2, -0.1, 1.0),
            sp(0.0, 0.15, 1.0),
            sp(0.01, 0.02, 1.0 + 1e-13),
        );
        assert!(
            (flat.closest.norm() - 1.0).abs() < 1e-9,
            "flat tetra must yield the plane distance, got {}",
            flat.closest.norm()
        );
    }

    #[test]
    fn a_degenerate_triangle_falls_back_to_its_edges() {
        // Collinear points (a repeated support direction under plateau ties):
        // the interior-region arithmetic divides by a vanishing area, so the
        // guard must reduce to the best edge, here the segment through x=0.4.
        let tri = closest_triangle(sp(0.4, -1.0, 0.0), sp(0.4, 1.0, 0.0), sp(0.4, 0.0, 0.0));
        assert!(
            (tri.closest - Vector3::new(0.4, 0.0, 0.0)).norm() < 1e-12,
            "collinear triangle must project onto its segment, got {:?}",
            tri.closest
        );
    }

    /// Axis-aligned box of half-extent `h` centered at `c`, as eight vertices.
    fn box_hull(c: Point3<f64>, h: f64, radius: f64) -> Hull {
        let mut verts = Vec::new();
        for sx in [-1.0, 1.0] {
            for sy in [-1.0, 1.0] {
                for sz in [-1.0, 1.0] {
                    verts.push(pt(c.x + sx * h, c.y + sy * h, c.z + sz * h));
                }
            }
        }
        Hull::new(&crate::hull::exact_hull(&verts).expect("box hull"), radius).expect("box")
    }

    #[test]
    fn separated_boxes_report_the_gap() {
        let a = box_hull(pt(0.0, 0.0, 0.0), 0.5, 0.0);
        let b = box_hull(pt(2.0, 0.0, 0.0), 0.5, 0.0);
        // Faces one apart at x = 0.5 and x = 1.5.
        assert!((distance(&a, &b).distance - 1.0).abs() < 1e-9);
        // Diagonal offset: nearest features are edges/corners.
        let c = box_hull(pt(2.0, 2.0, 0.0), 0.5, 0.0);
        let expected = ((2.0f64 - 1.0).powi(2) * 2.0).sqrt();
        assert!((distance(&a, &c).distance - expected).abs() < 1e-9);
    }

    #[test]
    fn rounded_boxes_subtract_radius() {
        let a = box_hull(pt(0.0, 0.0, 0.0), 0.5, 0.1);
        let b = box_hull(pt(2.0, 0.0, 0.0), 0.5, 0.15);
        assert!((distance(&a, &b).distance - (1.0 - 0.25)).abs() < 1e-9);
    }

    #[test]
    fn epa_recovers_depth_and_direction() {
        // Two unit cubes overlapping 0.3 on x (less than on y or z): the minimum
        // translation is 0.3 along x, and the witnesses separate along x by that.
        let a = box_hull(pt(0.0, 0.0, 0.0), 0.5, 0.0); // [-0.5, 0.5]^3
        let b = box_hull(pt(0.7, 0.0, 0.0), 0.5, 0.0); // [0.2, 1.2] on x
        let r = distance(&a, &b);
        assert!(
            (r.distance + 0.3).abs() < 1e-6,
            "penetration depth ~0.3, got {}",
            -r.distance
        );
        let sep = r.on_a - r.on_b;
        assert!(
            sep.x.abs() > 0.29 && sep.y.abs() < 1e-6 && sep.z.abs() < 1e-6,
            "escape is along x, got {sep:?}"
        );
    }

    #[test]
    fn epa_penetration_runs_through_the_radii() {
        // Cores overlap 0.7 on x; radii 0.1 + 0.2 deepen it to 1.0.
        let a = box_hull(pt(0.0, 0.0, 0.0), 0.5, 0.1);
        let b = box_hull(pt(0.3, 0.0, 0.0), 0.5, 0.2);
        let r = distance(&a, &b);
        assert!(
            (r.distance + 1.0).abs() < 1e-6,
            "expected -1.0, got {}",
            r.distance
        );
    }

    #[test]
    fn distance_is_continuous_through_contact() {
        // Mesh hulls are never perfectly symmetric; an irregular blob swept
        // apart has a signed distance that rises smoothly through zero (EPA
        // below, GJK above), with no jump at contact.
        let blob = |c: Point3<f64>| {
            let mut rng = rand::rngs::StdRng::seed_from_u64(4);
            let verts: Vec<_> = (0..40)
                .map(|_| {
                    Point3::new(
                        c.x + rng.random_range(-0.5..0.5),
                        c.y + rng.random_range(-0.5..0.5),
                        c.z + rng.random_range(-0.5..0.5),
                    )
                })
                .collect();
            Hull::new(&crate::hull::exact_hull(&verts).expect("hull"), 0.0).expect("blob")
        };
        let a = blob(pt(0.0, 0.0, 0.0));
        let (mut prev, mut crossed) = (None, false);
        for k in 0..=40 {
            let x = 0.2 + k as f64 * 0.03; // deep overlap through to separation
            let d = distance(&a, &blob(pt(x, 0.0, 0.0))).distance;
            if let Some(p) = prev {
                assert!(
                    d >= p - 1e-6,
                    "distance jumped backward: {p} then {d} at x={x}"
                );
                assert!(
                    d - p < 0.06,
                    "distance jumped forward: {p} then {d} at x={x}"
                );
                crossed |= p < 0.0 && d >= 0.0;
            }
            prev = Some(d);
        }
        assert!(crossed, "the sweep should pass through contact");
    }

    #[test]
    fn placed_distance_is_isometry_invariant() {
        // Placing both hulls by the same isometry leaves their distance fixed.
        let a = box_hull(pt(0.0, 0.0, 0.0), 0.4, 0.1);
        let b = box_hull(pt(1.5, 0.3, -0.2), 0.4, 0.15);
        let iso = Isometry3::from_parts(
            Translation3::new(0.3, -2.0, 1.7),
            UnitQuaternion::from_euler_angles(0.4, -0.9, 1.3),
        );
        let before = distance(&a, &b).distance;
        let after = distance(&Placed::new(&a, iso), &Placed::new(&b, iso)).distance;
        assert!((before - after).abs() < 1e-9, "{before} vs {after}");
    }

    #[test]
    fn is_symmetric() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(3);
        for _ in 0..500 {
            let a = box_hull(
                pt(
                    rng.random_range(-1.0..1.0),
                    rng.random_range(-1.0..1.0),
                    0.0,
                ),
                0.4,
                0.05,
            );
            let b = box_hull(
                pt(
                    rng.random_range(-1.0..1.0),
                    rng.random_range(-1.0..1.0),
                    1.0,
                ),
                0.4,
                0.05,
            );
            let ab = distance(&a, &b).distance;
            let ba = distance(&b, &a).distance;
            assert!((ab - ba).abs() < 1e-9, "asymmetric: {ab} vs {ba}");
        }
    }

    #[test]
    fn converges_quickly_on_simple_primitives() {
        // A handful of iterations, never the cap, for well-separated convexes.
        let a = box_hull(pt(0.0, 0.0, 0.0), 0.5, 0.0);
        let b = box_hull(pt(3.0, 1.0, 0.5), 0.5, 0.0);
        assert!(distance(&a, &b).iterations < 10);
    }
}
