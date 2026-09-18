//! Axis-aligned clip regions: the decomposition primitive for a concave body.
//! A single convex hull of a concave mesh bridges its features into one bulging
//! solid, so a caller instead names regions of the body; each region's clipped
//! mesh surface is fitted with the same rounded simplified hull the auto-fit
//! gives every link, and the union of pieces bounds the body tightly.

use srs_model::nalgebra::Point3;

/// An axis-aligned region `[min, max]` a body's mesh is clipped to before hull
/// fitting, in the body's fit frame (root frame for a world-fixed body, link
/// frame for a chain link). A bound may be infinite to leave that side open.
/// Construction rejects NaN bounds and empty spans, so a held `ClipRegion`
/// always spans a volume.
///
/// The fitted pieces must jointly contain the body's whole mesh surface
/// (checked at build), and the check certifies each ~1 mm face patch inside a
/// single piece, so adjacent regions should overlap by a few millimetres
/// rather than share exact cut planes. A bound resting exactly on a large flat
/// mesh face captures that face into the region's hull; sit bounds ~1 mm off
/// such planes.
#[derive(Clone, Copy, Debug)]
pub struct ClipRegion {
    min: Point3<f64>,
    max: Point3<f64>,
}

impl ClipRegion {
    /// A region spanning `[min, max]` per axis; infinite bounds leave that side
    /// open. Errors on a NaN bound or an empty span (`min >= max` on any axis).
    pub fn new(min: Point3<f64>, max: Point3<f64>) -> Result<ClipRegion, String> {
        for axis in 0..3 {
            let (lo, hi) = (min[axis], max[axis]);
            if lo.is_nan() || hi.is_nan() {
                return Err(format!("clip region has a NaN bound on axis {axis}"));
            }
            if lo >= hi {
                return Err(format!(
                    "clip region spans nothing on axis {axis}: min {lo} >= max {hi}"
                ));
            }
        }
        Ok(ClipRegion { min, max })
    }

    /// The region's slice of the mesh surface: every triangle of the soup
    /// (each three points one triangle) clipped to the region's planes, as the
    /// deduplicated, deterministically ordered clipped-polygon vertices.
    /// Boundary-inclusive, so a triangle lying in a bound plane is kept and
    /// adjoining regions both cover it.
    pub(crate) fn clip_triangles(&self, soup: &[Point3<f64>]) -> Vec<Point3<f64>> {
        let mut points: Vec<Point3<f64>> = soup
            .chunks_exact(3)
            .flat_map(|tri| self.clip_triangle(tri))
            .collect();
        points.sort_by(|p, q| {
            p.x.total_cmp(&q.x)
                .then_with(|| p.y.total_cmp(&q.y))
                .then_with(|| p.z.total_cmp(&q.z))
        });
        points.dedup();
        points
    }

    /// Sutherland-Hodgman: the triangle successively clipped against each
    /// finite bound plane. Empty once the polygon falls entirely outside.
    fn clip_triangle(&self, tri: &[Point3<f64>]) -> Vec<Point3<f64>> {
        let mut poly = tri.to_vec();
        for axis in 0..3 {
            for (bound, keep_below) in [(self.min[axis], false), (self.max[axis], true)] {
                if bound.is_infinite() {
                    continue;
                }
                if poly.is_empty() {
                    return poly;
                }
                poly = clip_polygon(&poly, axis, bound, keep_below);
            }
        }
        poly
    }
}

/// One polygon against one axis-aligned half-space: vertices on the kept side
/// (boundary inclusive) survive, and each edge crossing the plane contributes
/// its intersection point. The crossing interpolation never divides by zero:
/// an edge with equal `axis` coordinates is entirely on one side.
fn clip_polygon(
    poly: &[Point3<f64>],
    axis: usize,
    bound: f64,
    keep_below: bool,
) -> Vec<Point3<f64>> {
    let inside = |p: &Point3<f64>| {
        if keep_below {
            p[axis] <= bound
        } else {
            p[axis] >= bound
        }
    };
    let mut out = Vec::with_capacity(poly.len() + 1);
    for (i, a) in poly.iter().enumerate() {
        let b = &poly[(i + 1) % poly.len()];
        if inside(a) {
            out.push(*a);
        }
        if inside(a) != inside(b) {
            let t = (bound - a[axis]) / (b[axis] - a[axis]);
            out.push(Point3::from(a.coords + t * (b.coords - a.coords)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const INF: f64 = f64::INFINITY;

    fn pt(x: f64, y: f64, z: f64) -> Point3<f64> {
        Point3::new(x, y, z)
    }

    fn region(min: [f64; 3], max: [f64; 3]) -> ClipRegion {
        ClipRegion::new(pt(min[0], min[1], min[2]), pt(max[0], max[1], max[2]))
            .expect("test region")
    }

    fn contains(points: &[Point3<f64>], p: Point3<f64>) -> bool {
        points.iter().any(|q| (q - p).norm() < 1e-12)
    }

    #[test]
    fn rejects_nan_and_empty_spans() {
        assert!(ClipRegion::new(pt(0.0, 0.0, f64::NAN), pt(1.0, 1.0, 1.0)).is_err());
        assert!(ClipRegion::new(pt(0.0, 0.0, 0.0), pt(1.0, 1.0, 0.0)).is_err());
        assert!(ClipRegion::new(pt(0.0, 2.0, 0.0), pt(1.0, 1.0, 1.0)).is_err());
        assert!(ClipRegion::new(pt(-INF, -INF, -INF), pt(INF, INF, INF)).is_ok());
    }

    #[test]
    fn a_triangle_inside_passes_unchanged() {
        let tri = [pt(0.1, 0.1, 0.1), pt(0.4, 0.1, 0.1), pt(0.1, 0.4, 0.1)];
        let out = region([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]).clip_triangles(&tri);
        assert_eq!(out.len(), 3);
        for v in tri {
            assert!(contains(&out, v), "corner {v:?} lost");
        }
    }

    #[test]
    fn a_triangle_outside_clips_to_nothing() {
        let tri = [pt(2.0, 0.0, 0.0), pt(3.0, 0.0, 0.0), pt(2.0, 1.0, 0.0)];
        let out = region([0.0, -1.0, -1.0], [1.0, 1.0, 1.0]).clip_triangles(&tri);
        assert!(out.is_empty());
    }

    #[test]
    fn a_straddling_triangle_gains_the_cut_points() {
        // One corner beyond x = 1: the clip keeps the two inside corners and
        // adds the two edge crossings, exactly on the plane.
        let tri = [pt(0.0, 0.0, 0.0), pt(2.0, 0.0, 0.0), pt(0.0, 2.0, 0.0)];
        let out = region([-INF, -INF, -INF], [1.0, INF, INF]).clip_triangles(&tri);
        assert_eq!(out.len(), 4);
        assert!(contains(&out, pt(0.0, 0.0, 0.0)));
        assert!(contains(&out, pt(0.0, 2.0, 0.0)));
        assert!(contains(&out, pt(1.0, 0.0, 0.0)));
        assert!(contains(&out, pt(1.0, 1.0, 0.0)));
    }

    #[test]
    fn both_neighbours_keep_a_triangle_on_their_shared_plane() {
        let tri = [pt(0.5, 0.0, 0.0), pt(0.5, 1.0, 0.0), pt(0.5, 0.0, 1.0)];
        let below = region([-INF, -INF, -INF], [0.5, INF, INF]).clip_triangles(&tri);
        let above = region([0.5, -INF, -INF], [INF, INF, INF]).clip_triangles(&tri);
        assert_eq!(below.len(), 3, "coplanar triangle kept below the cut");
        assert_eq!(above.len(), 3, "coplanar triangle kept above the cut");
    }

    #[test]
    fn a_degenerate_sliver_keeps_its_boundary_points() {
        // Collinear soup: no area to clip, but the surviving points still feed
        // the hull cloud so the sliver's extent is not lost.
        let tri = [pt(0.0, 0.0, 0.0), pt(2.0, 0.0, 0.0), pt(1.0, 0.0, 0.0)];
        let out = region([-INF, -INF, -INF], [1.5, INF, INF]).clip_triangles(&tri);
        assert!(contains(&out, pt(0.0, 0.0, 0.0)));
        assert!(contains(&out, pt(1.0, 0.0, 0.0)));
        assert!(contains(&out, pt(1.5, 0.0, 0.0)), "cut point on the sliver");
    }

    #[test]
    fn every_clipped_point_lies_in_the_region_and_extremes_survive() {
        // A cube surface clipped to its upper half: all output within bounds,
        // and the cut ring at z = 0.5 present.
        let mut soup = Vec::new();
        let faces = [
            [
                pt(0., 0., 0.),
                pt(1., 0., 0.),
                pt(1., 1., 0.),
                pt(0., 1., 0.),
            ],
            [
                pt(0., 0., 1.),
                pt(1., 0., 1.),
                pt(1., 1., 1.),
                pt(0., 1., 1.),
            ],
            [
                pt(0., 0., 0.),
                pt(1., 0., 0.),
                pt(1., 0., 1.),
                pt(0., 0., 1.),
            ],
            [
                pt(0., 1., 0.),
                pt(1., 1., 0.),
                pt(1., 1., 1.),
                pt(0., 1., 1.),
            ],
            [
                pt(0., 0., 0.),
                pt(0., 1., 0.),
                pt(0., 1., 1.),
                pt(0., 0., 1.),
            ],
            [
                pt(1., 0., 0.),
                pt(1., 1., 0.),
                pt(1., 1., 1.),
                pt(1., 0., 1.),
            ],
        ];
        for [a, b, c, d] in faces {
            soup.extend([a, b, c, a, c, d]);
        }
        let out = region([-INF, -INF, 0.5], [INF, INF, INF]).clip_triangles(&soup);
        assert!(out.iter().all(|p| p.z >= 0.5), "a point escaped the clip");
        for corner in [
            pt(0., 0., 1.),
            pt(1., 1., 1.),
            pt(0., 0., 0.5),
            pt(1., 1., 0.5),
        ] {
            assert!(contains(&out, corner), "extreme {corner:?} missing");
        }
    }
}
