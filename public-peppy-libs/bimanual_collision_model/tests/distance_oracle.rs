//! Independent oracle for the model's distance query: support-function bounds
//! computed with plain vertex arithmetic, sharing no code with the GJK/EPA
//! implementation under test.
//!
//! For two convex hulls with rounding radii, every unit direction `d` yields a
//! separation lower bound `min_a(d.a) - max_b(d.b) - ra - rb`, and every vertex
//! pair yields a distance upper bound `|va - vb| - ra - rb` (vertex-to-vertex
//! never underestimates the surface gap of the rounded hulls). A sampled
//! direction fan therefore brackets the true distance from both sides:
//!
//! - reported `d > 0` must sit inside `[max separation - slack, min pair]`,
//!   and its witnesses must span exactly `d`;
//! - reported `d <= 0` (EPA) requires that no sampled direction separates the
//!   hulls (a separating direction would prove the penetration fabricated),
//!   and the depth must not exceed the directional overlap along any sample.
//!
//! The fan's angular resolution bounds the slack: for hulls inside radius `R`,
//! a direction within `theta` of the true separating normal underestimates by
//! at most `2 R (1 - cos theta)`. The fabricated-penetration bug class this
//! guards against (a stateful support walk plus a degenerate simplex read a
//! wrist 74 mm clear of a torso slab as 90 mm inside it) sat five orders of
//! magnitude outside these brackets.

use bimanual_collision_model::{BimanualCollisionModel, PlacedPiece};
use srs_model::JointVec;
use srs_model::nalgebra::Vector3;

#[path = "fixtures/openarm.rs"]
mod openarm;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

/// Fibonacci-sphere direction count. 2000 directions give ~4.4e-3 sr per cell
/// (angular radius ~2.2 degrees), so the separation lower bound is tight to
/// about `2 R (1 - cos 2.2deg) ~= 0.3 mm` for these sub-metre hulls.
const DIRECTIONS: usize = 2000;

/// Slack absorbing the direction fan's angular resolution plus float noise.
const ORACLE_SLACK: f64 = 1.5e-3;

fn model() -> BimanualCollisionModel {
    BimanualCollisionModel::builder_from_file(
        &format!("{FIXTURES}/openarm_v10.urdf"),
        &format!("{FIXTURES}/meshes"),
        "openarm_left_link0",
        "openarm_right_link0",
    )
    .expect("read fixture urdf")
    .regions(openarm::TORSO_BODY, openarm::torso_regions())
    .build()
    .expect("fixture model")
}

fn fibonacci_directions() -> Vec<Vector3<f64>> {
    (0..DIRECTIONS)
        .map(|i| {
            let phi = i as f64 * 0.618_033_988_749 * std::f64::consts::TAU;
            let z = -1.0 + 2.0 * (i as f64 + 0.5) / DIRECTIONS as f64;
            let r = (1.0 - z * z).sqrt();
            Vector3::new(r * phi.cos(), r * phi.sin(), z)
        })
        .collect()
}

/// Support interval of one body's pieces along `d`: (min, max) over all
/// vertices of all pieces, widened by each piece's rounding radius.
fn support_interval(pieces: &[PlacedPiece], d: &Vector3<f64>) -> (f64, f64) {
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for p in pieces {
        for v in &p.vertices {
            let s = v.coords.dot(d);
            lo = lo.min(s - p.radius);
            hi = hi.max(s + p.radius);
        }
    }
    (lo, hi)
}

/// Smallest vertex-pair surface distance between two bodies' rounded pieces:
/// an upper bound on the true surface distance.
fn vertex_pair_upper_bound(a: &[PlacedPiece], b: &[PlacedPiece]) -> f64 {
    let mut best = f64::INFINITY;
    for pa in a {
        for pb in b {
            for va in &pa.vertices {
                for vb in &pb.vertices {
                    best = best.min((va - vb).norm() - pa.radius - pb.radius);
                }
            }
        }
    }
    best
}

#[test]
fn reported_distances_sit_inside_independent_support_bounds() {
    let mut m = model();
    let dirs = fibonacci_directions();

    // Pose sweep spanning clear, in-band, and penetrating regimes, at three
    // openings so the live finger placement is inside the check.
    let mut poses: Vec<(JointVec, JointVec)> = Vec::new();
    for t in [0.0, 0.3, 0.6, 0.9, 1.05, 1.2, 1.5] {
        poses.push((
            [0.0, 0.0, t, 0.4, 0.0, 0.0, 0.0],
            [0.0, 0.0, -t, 0.4, 0.0, 0.0, 0.0],
        ));
    }
    for t in [0.2, 0.5, 0.8] {
        poses.push((
            [0.15, 0.1, t, 0.5, -0.2, 0.1, 0.0],
            [-0.05, -0.25, -t, 0.35, 0.1, -0.1, 0.0],
        ));
        poses.push((
            [0.0, t * 0.5, 0.9, 0.45, 0.1, 0.0, 0.2],
            [0.0, -t * 0.5, -1.05, 0.4, -0.1, 0.1, 0.0],
        ));
    }

    for opening in [0.0, 0.5, 1.0] {
        m.set_gripper_openings(opening, opening);
        for (ql, qr) in &poses {
            let (d, link_a, link_b, on_a, on_b) = {
                let p = m.min_distance(ql, qr).expect("query");
                (
                    p.distance,
                    p.link_a.to_string(),
                    p.link_b.to_string(),
                    p.on_a,
                    p.on_b,
                )
            };
            let pieces = m.world_pieces(ql, qr).expect("pieces");
            let of = |name: &str| -> &[PlacedPiece] {
                &pieces.iter().find(|(n, _)| *n == name).expect("body").1
            };
            let (a, b) = (of(&link_a), of(&link_b));

            let max_separation = dirs
                .iter()
                .map(|dir| {
                    let (a_lo, _) = support_interval(a, dir);
                    let (_, b_hi) = support_interval(b, dir);
                    a_lo - b_hi
                })
                .fold(f64::NEG_INFINITY, f64::max);
            let context = format!(
                "pose ql={ql:?} qr={qr:?} opening {opening}: {link_a} vs {link_b} d={d:+.5}"
            );

            if d > 0.0 {
                let upper = vertex_pair_upper_bound(a, b);
                assert!(
                    d >= max_separation - ORACLE_SLACK,
                    "{context}: below the separation lower bound {max_separation:+.5}"
                );
                assert!(
                    d <= upper + ORACLE_SLACK,
                    "{context}: above the vertex-pair upper bound {upper:+.5}"
                );
                let witness_gap = (on_a - on_b).norm();
                assert!(
                    (witness_gap - d).abs() < 1e-6,
                    "{context}: witnesses span {witness_gap:+.5}, not the reported distance"
                );
            } else {
                // A single separating direction would prove the reported
                // penetration fabricated.
                assert!(
                    max_separation <= ORACLE_SLACK,
                    "{context}: a sampled direction separates by {max_separation:+.5}"
                );
                // Depth cannot exceed the overlap along any sampled direction.
                let min_overlap = -max_separation;
                assert!(
                    -d <= min_overlap + ORACLE_SLACK,
                    "{context}: depth exceeds the directional overlap {min_overlap:+.5}"
                );
            }
        }
    }
}

#[test]
fn every_checked_pair_matches_the_oracle_at_a_near_contact_pose() {
    // The min-distance test above only audits the winning pair; a defect on a
    // non-winning pair could bias which pair wins. At one adversarial pose
    // (wrists converging, mid opening), audit every checked pair against the
    // direction fan.
    let mut m = model();
    m.set_gripper_openings(0.5, 0.5);
    let ql: JointVec = [0.0, 0.0, 0.95, 0.4, 0.1, 0.0, 0.2];
    let qr: JointVec = [0.0, 0.0, -1.05, 0.4, -0.1, 0.1, 0.0];

    let dirs = fibonacci_directions();
    let checked: Vec<(String, String)> = m
        .checked_pairs()
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    // The property the runtime relies on: the reported minimum can exceed no
    // pair's true distance. For a pair the direction fan certifies as
    // separated, the vertex-pair bound is a valid upper bound on that (then
    // positive) distance, so a reported minimum above it means the model
    // missed a closer pair.
    let reported = m.min_distance(&ql, &qr).expect("query").distance;
    let pieces = m.world_pieces(&ql, &qr).expect("pieces");
    let of = |name: &str| -> &[PlacedPiece] {
        &pieces.iter().find(|(n, _)| *n == name).expect("body").1
    };
    for (la, lb) in &checked {
        let max_sep = dirs
            .iter()
            .map(|dir| {
                let (a_lo, _) = support_interval(of(la), dir);
                let (_, b_hi) = support_interval(of(lb), dir);
                a_lo - b_hi
            })
            .fold(f64::NEG_INFINITY, f64::max);
        if max_sep <= ORACLE_SLACK {
            // Possibly overlapping: vertex-pair gaps do not bound a signed
            // penetration depth, so this pair cannot be audited from above.
            continue;
        }
        let upper = vertex_pair_upper_bound(of(la), of(lb));
        assert!(
            upper >= max_sep - 2.0 * ORACLE_SLACK,
            "{la} vs {lb}: oracle bracket inverted ({max_sep:+.5} > {upper:+.5})"
        );
        assert!(
            reported <= upper + ORACLE_SLACK,
            "{la} vs {lb}: reported min {reported:+.5} exceeds this pair's upper bound {upper:+.5}: a closer pair was missed"
        );
    }
}

/// The exact regression scene from the field: a near-home converging pose read
/// as deep penetration by a defective query path while the true clearance was
/// positive. Pin the bracket, not just the sign.
#[test]
fn field_regression_pose_reads_clear_within_oracle_brackets() {
    let mut m = model();
    m.set_gripper_openings(0.0, 0.0);
    let ql: JointVec = [0.0, 0.0, 0.1075, 0.1575, 0.0, 0.0, 0.0];
    let qr: JointVec = [0.0, 0.0, -0.1075, 0.1575, 0.0, 0.0, 0.0];
    let (d, la, lb) = {
        let p = m.min_distance(&ql, &qr).expect("query");
        (p.distance, p.link_a.to_string(), p.link_b.to_string())
    };
    let dirs = fibonacci_directions();
    let pieces = m.world_pieces(&ql, &qr).expect("pieces");
    let of = |name: &str| -> &[PlacedPiece] {
        &pieces.iter().find(|(n, _)| *n == name).expect("body").1
    };
    let max_sep = dirs
        .iter()
        .map(|dir| {
            let (a_lo, _) = support_interval(of(&la), dir);
            let (_, b_hi) = support_interval(of(&lb), dir);
            a_lo - b_hi
        })
        .fold(f64::NEG_INFINITY, f64::max);
    assert!(
        d > 0.0 && d >= max_sep - ORACLE_SLACK,
        "field pose must read clear within the oracle bracket, got {d:+.5} (bracket lower {max_sep:+.5})"
    );
}
