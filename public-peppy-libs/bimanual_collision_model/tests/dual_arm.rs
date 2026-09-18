//! Integration tests: the OpenArm fixture model driven through two-arm
//! scenarios. Distances are signed hull distances (GJK, EPA on overlap), so a
//! change to the fixture geometry or the fit moves these numbers. Most assertions
//! are qualitative (clear vs colliding, monotone, which links) so they survive a
//! re-fit; `rest_pose_clearance_is_stable` is the deliberate exception, pinning the
//! rest clearance as a regression guard (update its constant on an intended re-fit).

use bimanual_collision_model::{BimanualCollisionModel, PlacedPiece};
use srs_model::JointVec;
use srs_model::nalgebra::{Point3, Vector3};

#[path = "fixtures/openarm.rs"]
mod openarm;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

/// In-limit home: the elbow's one-sided lower limit is 0.05.
const HOME: JointVec = [0.0, 0.0, 0.0, 0.05, 0.0, 0.0, 0.0];

/// The fixture model under the tight torso regions we actually ship; the
/// auto-fit torso hull bulges across the chest and clips the grippers at rest,
/// so the integration scenarios run against the supplied decomposition instead.
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

/// Both arms elbow-bent, j3 wrapping the wrists toward the centerline.
fn wrists_inward(t: f64) -> (JointVec, JointVec) {
    let mut ql: JointVec = [0.0, 0.0, 0.0, 0.4, 0.0, 0.0, 0.0];
    let mut qr = ql;
    ql[2] = t;
    qr[2] = -t;
    (ql, qr)
}

#[test]
fn wrists_converging_monotonically_reach_collision() {
    let mut m = model();
    let mut prev = f64::INFINITY;
    let mut last = (String::new(), String::new(), 0.0);
    for i in 0..=12 {
        let (ql, qr) = wrists_inward(i as f64 * 0.1);
        let p = m.min_distance(&ql, &qr).expect("query");
        assert!(
            p.distance <= prev + 1e-3,
            "approach not monotone: {:+.4} after {prev:+.4}",
            p.distance
        );
        prev = p.distance;
        last = (p.link_a.to_string(), p.link_b.to_string(), p.distance);
    }
    let (a, b, d) = last;
    assert!(
        d < 0.0,
        "fully wrapped wrists should interpenetrate, got {d:+.4}"
    );
    // The gripper region: the wrist link (link7) or one of its finger bodies. A
    // wrapped wrist drives its outstretched finger into contact, so the deepest
    // witness is typically a finger rather than the bare wrist hull.
    let gripper_region = |l: &str| l.contains("link7") || l.contains("finger");
    assert!(
        gripper_region(&a) || gripper_region(&b),
        "deepest pair should involve a wrist or its finger, got {a} vs {b}"
    );
}

#[test]
fn closing_the_gripper_recovers_clearance() {
    // The core reason per-finger live hulls exist: when the two grippers approach,
    // closing the fingers pulls them back from the fat fully-open envelope, so the
    // reported clearance grows and the arms can come closer before the governor
    // stops them. Same pose, only the opening changes.
    let mut m = model();
    let (ql, qr) = wrists_inward(0.9);

    m.set_gripper_openings(1.0, 1.0); // fully open: widest finger envelope
    let (open, open_a, open_b) = {
        let p = m.min_distance(&ql, &qr).expect("query");
        (p.distance, p.link_a.to_string(), p.link_b.to_string())
    };
    assert!(
        open_a.contains("finger") || open_b.contains("finger"),
        "setup: a finger should be the nearest body when open, got {open_a} vs {open_b}"
    );

    m.set_gripper_openings(0.0, 0.0); // fully closed: fingers retract
    let closed = m.min_distance(&ql, &qr).expect("query").distance;

    assert!(
        closed > open + 1e-4,
        "closing the fingers should recover clearance: open {open:+.4}, closed {closed:+.4}"
    );

    // Out-of-range openings clamp to the travel: same placement as the extremes.
    m.set_gripper_openings(-0.5, -0.5);
    let clamped = m.min_distance(&ql, &qr).expect("query").distance;
    assert!(
        (clamped - closed).abs() < 1e-12,
        "an out-of-range opening should clamp to the nearest extreme"
    );
}

#[test]
fn mirrored_limit_fingers_place_identically() {
    // OpenArm v2 mirrors its right gripper by flipping the finger joints' limit
    // range ([-x, 0] instead of [0, x]) rather than the axis sign, so on that
    // side the LOWER limit is the open end. Rewrite the fixture's right fingers
    // in that style (limits negated, axes flipped: the identical physical
    // motion) and pin that live placement matches the original at every
    // opening: the model must read the open end off the meshes, not off the
    // URDF limit order, or "fully open" parks the mirrored side's fingers
    // closed.
    let urdf = std::fs::read_to_string(format!("{FIXTURES}/openarm_v10.urdf")).expect("fixture");
    let rewrites = [
        (
            "<child link=\"openarm_right_right_finger\"/>\n    <origin rpy=\"0 0 0\" xyz=\"0.0 0.0 0.1025\"/>\n    <axis xyz=\"0 -1 0\"/>\n    <limit effort=\"333\" lower=\"0.0\" upper=\"0.044\" velocity=\"10.0\"/>",
            "<child link=\"openarm_right_right_finger\"/>\n    <origin rpy=\"0 0 0\" xyz=\"0.0 0.0 0.1025\"/>\n    <axis xyz=\"0 1 0\"/>\n    <limit effort=\"333\" lower=\"-0.044\" upper=\"0.0\" velocity=\"10.0\"/>",
        ),
        (
            "<child link=\"openarm_right_left_finger\"/>\n    <origin rpy=\"0 0 0\" xyz=\"0.0 -0.0 0.1025\"/>\n    <axis xyz=\"0 1 0\"/>\n    <limit effort=\"333\" lower=\"0.0\" upper=\"0.044\" velocity=\"10.0\"/>",
            "<child link=\"openarm_right_left_finger\"/>\n    <origin rpy=\"0 0 0\" xyz=\"0.0 -0.0 0.1025\"/>\n    <axis xyz=\"0 -1 0\"/>\n    <limit effort=\"333\" lower=\"-0.044\" upper=\"0.0\" velocity=\"10.0\"/>",
        ),
    ];
    let mirrored = rewrites.iter().fold(urdf.clone(), |acc, (from, to)| {
        assert!(
            acc.contains(from),
            "fixture drifted from the rewrite anchor"
        );
        acc.replacen(from, to, 1)
    });

    let build = |urdf: &str| {
        BimanualCollisionModel::builder(
            urdf,
            &format!("{FIXTURES}/meshes"),
            "openarm_left_link0",
            "openarm_right_link0",
        )
        .regions(openarm::TORSO_BODY, openarm::torso_regions())
        .build()
        .expect("model")
    };
    let mut original = build(&urdf);
    let mut flipped = build(&mirrored);

    // Separation of the two right-finger body centroids in world frame.
    let separation = |m: &mut BimanualCollisionModel, fraction: f64| -> f64 {
        m.set_gripper_openings(fraction, fraction);
        let pieces = m.world_pieces(&HOME, &HOME).expect("pieces");
        let centroid = |name: &str| {
            let (_, ps) = pieces
                .iter()
                .find(|(n, _)| *n == name)
                .expect("finger body exists");
            let verts: Vec<_> = ps.iter().flat_map(|p| p.vertices.iter()).collect();
            verts.iter().fold(
                bimanual_collision_model::nalgebra::Vector3::zeros(),
                |a, v| a + v.coords,
            ) / verts.len() as f64
        };
        (centroid("openarm_right_right_finger") - centroid("openarm_right_left_finger")).norm()
    };

    for fraction in [0.0, 0.5, 1.0] {
        let a = separation(&mut original, fraction);
        let b = separation(&mut flipped, fraction);
        assert!(
            (a - b).abs() < 1e-9,
            "mirrored-limit placement diverged at fraction {fraction}: {a:.5} vs {b:.5}"
        );
    }
    assert!(
        separation(&mut flipped, 1.0) > separation(&mut flipped, 0.0) + 0.05,
        "fraction 1.0 must spread the fingers"
    );
}

#[test]
fn mixed_flip_finger_limits_fail_the_build() {
    // Rewrite only ONE right finger to the mirrored [-x, 0] limit style (its
    // sibling keeps [0, x]): each finger's own motion is physically unchanged,
    // but the pair's limit ranges now disagree on a closed-to-open direction, so
    // the tip separation is equal at both limit extremes and any orientation
    // would be picked by floating-point noise. The build must refuse loudly
    // rather than guess and silently place "open" as closed on one finger.
    let urdf = std::fs::read_to_string(format!("{FIXTURES}/openarm_v10.urdf")).expect("fixture");
    let from = "<child link=\"openarm_right_left_finger\"/>\n    <origin rpy=\"0 0 0\" xyz=\"0.0 -0.0 0.1025\"/>\n    <axis xyz=\"0 1 0\"/>\n    <limit effort=\"333\" lower=\"0.0\" upper=\"0.044\" velocity=\"10.0\"/>";
    let to = "<child link=\"openarm_right_left_finger\"/>\n    <origin rpy=\"0 0 0\" xyz=\"0.0 -0.0 0.1025\"/>\n    <axis xyz=\"0 -1 0\"/>\n    <limit effort=\"333\" lower=\"-0.044\" upper=\"0.0\" velocity=\"10.0\"/>";
    assert!(
        urdf.contains(from),
        "fixture drifted from the rewrite anchor"
    );
    let mixed = urdf.replacen(from, to, 1);
    let err = BimanualCollisionModel::builder(
        &mixed,
        &format!("{FIXTURES}/meshes"),
        "openarm_left_link0",
        "openarm_right_link0",
    )
    .regions(openarm::TORSO_BODY, openarm::torso_regions())
    .build()
    .err()
    .expect("a mixed-flip gripper must fail the build");
    assert!(
        err.to_string().contains("ambiguous"),
        "expected the ambiguous-orientation error, got: {err}"
    );
}

#[test]
fn folding_the_arms_inward_drives_a_collision() {
    let mut m = model();
    // Mirrored j2 folds both arms toward the centerline, into the torso.
    let ql: JointVec = [0.0, 0.6, 0.0, 0.4, 0.0, 0.0, 0.0];
    let qr: JointVec = [0.0, -0.6, 0.0, 0.4, 0.0, 0.0, 0.0];
    let p = m.min_distance(&ql, &qr).expect("query");
    assert!(
        p.distance < 0.0,
        "folded arms should interpenetrate, got {:+.4}",
        p.distance
    );
    let touches_arm = [p.link_a, p.link_b]
        .iter()
        .any(|l| l.contains("link3") || l.contains("link4") || l.contains("body"));
    assert!(
        touches_arm,
        "expected an upper-arm or torso witness, got {} vs {}",
        p.link_a, p.link_b
    );
}

#[test]
fn rest_pose_clearance_is_stable() {
    // The auto-fit torso bulges a phantom slab that reads a false near-contact
    // against the grippers at rest; the tight shipped regions leave the true
    // clearance. Pinned to the measured value as a two-sided regression guard: a
    // re-fit that moves the rest clearance in either direction trips this, so update
    // the constant when the geometry deliberately changes.
    const REST_CLEARANCE_M: f64 = 0.0333;
    const TOLERANCE_M: f64 = 0.001;
    let mut m = model();
    let p = m.min_distance(&HOME, &HOME).expect("query");
    assert!(
        (p.distance - REST_CLEARANCE_M).abs() < TOLERANCE_M,
        "rest clearance {:+.5} drifted from {REST_CLEARANCE_M} (tol {TOLERANCE_M})",
        p.distance
    );
}

#[test]
fn queries_are_order_independent_near_the_torso_regions() {
    // A stateful support function (a warm-started graph climb) answered this
    // pose wrong whenever another pair's query ran first: the torso's clipped
    // slice hulls carry repair artifacts in their triangle graph, a greedy
    // climb parked on a false summit, and GJK reported the wrist 90 mm INSIDE
    // a torso slab it was actually 74 mm clear of. The pose sits on the wrists'
    // natural converging path, so the governor steered on the garbage reading.
    // Support is an exact scan; pin the true clearance and that repeating the
    // query after a full model sweep cannot change the answer.
    let mut m = model();
    let ql: JointVec = [0.0, 0.0, 0.1075, 0.1575, 0.0, 0.0, 0.0];
    let qr: JointVec = [0.0, 0.0, -0.1075, 0.1575, 0.0, 0.0, 0.0];
    let first = m.min_distance(&ql, &qr).expect("query").distance;
    assert!(
        first > 0.02,
        "near-home converging pose must read clear (~+0.024 true), got {first:+.5}"
    );
    let (deep_l, deep_r) = wrists_inward(1.2);
    m.min_distance(&deep_l, &deep_r).expect("query");
    let again = m.min_distance(&ql, &qr).expect("query").distance;
    assert!(
        (first - again).abs() < 1e-12,
        "query order changed the answer: {first:+.5} then {again:+.5}"
    );
}

#[test]
fn separating_sweep_increases_clearance() {
    // Sweeping j2 outward moves the arms apart, so the nearest-pair clearance only
    // grows (and plateaus once the binding pair stops closing). Stronger than a
    // fixed floor: it asserts the separation actually shows up as monotone,
    // positive clearance.
    let mut m = model();
    let mut prev = f64::NEG_INFINITY;
    for i in 0..=12 {
        let t = i as f64 * 0.1;
        let p = m
            .min_distance(
                &[0.0, -t, 0.0, 0.4, 0.0, 0.0, 0.0],
                &[0.0, t, 0.0, 0.4, 0.0, 0.0, 0.0],
            )
            .expect("query");
        assert!(
            p.distance > 0.0,
            "outward sweep should stay clear at t={t}, got {:+.4}",
            p.distance
        );
        assert!(
            p.distance >= prev - 1e-6,
            "outward sweep reduced clearance to {:+.4} at t={t}",
            p.distance
        );
        prev = p.distance;
    }
}

#[test]
fn witnesses_are_finite_and_span_the_gap_when_clear() {
    let mut m = model();
    // A clearly separated pose: the witnesses lie on the surfaces and their gap
    // equals the (positive) reported distance.
    let (ql, qr) = wrists_inward(0.3);
    let p = m.min_distance(&ql, &qr).expect("query");
    assert!(
        p.distance > 0.0,
        "pose should be clear, got {:+.4}",
        p.distance
    );
    for w in [p.on_a, p.on_b] {
        assert!(
            w.coords.iter().all(|c| c.is_finite() && c.abs() < 2.0),
            "witness {w:?} not plausible"
        );
    }
    assert!(
        ((p.on_a - p.on_b).norm() - p.distance).abs() < 1e-6,
        "witness gap vs distance {:+.4}",
        p.distance
    );
}

#[test]
fn in_collision_threshold_semantics() {
    let mut m = model();
    let rest = m.min_distance(&HOME, &HOME).expect("query").distance;
    assert!(m.in_collision(&HOME, &HOME, rest + 0.005).expect("query"));
    assert!(!m.in_collision(&HOME, &HOME, rest - 0.005).expect("query"));
    let (ql, qr) = wrists_inward(1.2);
    assert!(m.in_collision(&ql, &qr, 0.0).expect("query"));
}

#[test]
fn non_finite_configurations_are_rejected() {
    let mut m = model();
    let mut bad = HOME;
    bad[3] = f64::NAN;
    assert!(m.min_distance(&bad, &HOME).is_err());
    assert!(m.min_distance(&HOME, &bad).is_err());
    bad[3] = f64::INFINITY;
    assert!(m.in_collision(&HOME, &bad, 0.0).is_err());
}

/// A conservative lower bound on the distance from `p` to a placed piece's
/// rounded surface: the deepest face-plane violation of the core hull (each
/// half-space distance under-estimates the hull distance) minus the inflation
/// radius. Positive means provably outside the piece by at least that much.
fn outside_by_at_least(piece: &PlacedPiece, p: &Point3<f64>) -> f64 {
    let interior = Point3::from(
        piece
            .vertices
            .iter()
            .fold(Vector3::zeros(), |a, v| a + v.coords)
            / piece.vertices.len() as f64,
    );
    piece
        .faces
        .iter()
        .filter_map(|f| {
            let (a, b, c) = (
                piece.vertices[f[0]],
                piece.vertices[f[1]],
                piece.vertices[f[2]],
            );
            let n = (b - a).cross(&(c - a));
            if n.norm() < 1e-12 {
                return None;
            }
            let outward = if n.dot(&(interior - a)) > 0.0 { -n } else { n }.normalize();
            Some(outward.dot(&(p - a)))
        })
        .fold(f64::NEG_INFINITY, f64::max)
        - piece.radius
}

#[test]
fn old_box_phantom_volumes_are_outside_the_torso_pieces() {
    // Points the old hand-fitted torso boxes claimed but the mesh never
    // occupied: behind the gusset's hypotenuse (the old rear-brace box filled
    // the whole triangle above the diagonal), beside the tapered head top (the
    // old head box carried the widest extents to the very top), and above the
    // thin base plate (boxed 28 mm thick for an 8 mm plate). Each must now be
    // provably clear of every torso piece, so approaching them reads real
    // clearance instead of false proximity.
    let probes: [(Point3<f64>, f64); 3] = [
        (Point3::new(-0.10, 0.0, 0.15), 0.015),
        (Point3::new(-0.083, 0.075, 0.760), 0.020),
        (Point3::new(0.05, 0.05, 0.022), 0.003),
    ];
    let mut m = model();
    let pieces = m.world_pieces(&HOME, &HOME).expect("pieces");
    let torso: &Vec<PlacedPiece> = &pieces
        .iter()
        .find(|(name, _)| *name == openarm::TORSO_BODY)
        .expect("torso body present")
        .1;
    for (probe, margin) in probes {
        let clearance = torso
            .iter()
            .map(|piece| outside_by_at_least(piece, &probe))
            .fold(f64::INFINITY, f64::min);
        assert!(
            clearance >= margin,
            "phantom probe {probe:?} clears the pieces by {clearance:+.4}, need {margin}"
        );
    }
}

/// What the fit costs and buys, per piece and in total. Not an assertion: the
/// numbers are the deliverable, so run it deliberately.
///
/// ```sh
/// cargo test --release --test dual_arm fit_cost_report -- --ignored --nocapture
/// ```
#[test]
#[ignore = "measurement probe, not a pass/fail check"]
fn fit_cost_report() {
    let build_start = std::time::Instant::now();
    let mut m = model();
    let build = build_start.elapsed();

    let mut rows: Vec<(String, usize, usize, f64)> = m
        .world_pieces(&HOME, &HOME)
        .expect("pieces")
        .into_iter()
        .map(|(name, ps)| {
            let vertices = ps.iter().map(|p| p.vertices.len()).sum();
            let radius = ps.iter().map(|p| p.radius).fold(0.0, f64::max);
            (name.to_string(), ps.len(), vertices, radius)
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.2));
    let (pieces, vertices) = rows.iter().fold((0, 0), |(p, v), r| (p + r.1, v + r.2));
    let radius = rows.iter().map(|r| r.3).fold(0.0, f64::max);

    println!("\nbody                             pieces  vertices  radius (mm)");
    for (name, p, v, r) in &rows {
        println!("{name:32} {p:6} {v:9} {:12.3}", r * 1000.0);
    }
    println!(
        "{:32} {pieces:6} {vertices:9} {:12.3}",
        "TOTAL",
        radius * 1000.0
    );

    const ITERATIONS: usize = 2000;
    let mut acc = 0.0;

    // HOME first, at the openings the model defaults to, so the clearance
    // printed here is the same quantity `rest_pose_clearance_is_stable` pins and
    // can be read straight into its constant.
    let rest_clearance = m.min_distance(&HOME, &HOME).expect("query").distance;
    let rest = std::time::Instant::now();
    for _ in 0..ITERATIONS {
        acc += m.min_distance(&HOME, &HOME).expect("query").distance;
    }
    let per_rest = rest.elapsed().as_secs_f64() / ITERATIONS as f64;

    // An in-band pose with the jaws closed, the regime the governor actually
    // queries in. This overrides the openings, so it runs last.
    let (ql, qr) = (
        [0.0, 0.0, 1.2, 0.4, 0.0, 0.0, 0.0],
        [0.0, 0.0, -1.2, 0.4, 0.0, 0.0, 0.0],
    );
    m.set_gripper_openings(0.0, 0.0);
    let d = m.min_distance(&ql, &qr).expect("query").distance;
    let start = std::time::Instant::now();
    for _ in 0..ITERATIONS {
        acc += m.min_distance(&ql, &qr).expect("query").distance;
    }
    let per_query = start.elapsed().as_secs_f64() / ITERATIONS as f64;
    assert!(acc.is_finite());

    println!("separated (HOME) query {:.1} us", per_rest * 1e6);
    println!(
        "\nbuild {:.0} ms | in-band query {:.1} us (d={d:+.5} m) | rest clearance {rest_clearance:+.5} m\n",
        build.as_secs_f64() * 1e3,
        per_query * 1e6,
    );
}

/// Wall-clock budget: a full dual-arm query must stay far inside a control
/// tick. Debug builds are several times slower, so assert in release only.
#[test]
fn query_stays_inside_the_control_tick() {
    let mut m = model();
    let configs: Vec<(JointVec, JointVec)> =
        (0..200).map(|i| wrists_inward(i as f64 * 0.005)).collect();

    let start = std::time::Instant::now();
    let mut acc = 0.0;
    for (ql, qr) in &configs {
        acc += m.min_distance(ql, qr).expect("query").distance;
    }
    let per_query = start.elapsed().as_secs_f64() / configs.len() as f64;
    assert!(acc.is_finite());
    println!("per-query: {:.1} us", per_query * 1e6);
    if !cfg!(debug_assertions) {
        assert!(
            per_query < 1e-3,
            "query took {:.1} us, budget is 1 ms",
            per_query * 1e6
        );
    }
}
