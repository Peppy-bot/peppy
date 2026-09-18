//! End-to-end IK<->FK round trip through the public `Arm` API: a handful of
//! in-limit configurations are mapped to an EE pose by FK and recovered by the
//! closed-form IK, checking that the URDF -> FK -> model wiring is correct.
//! The exhaustive seeded-random sweep over the configuration space lives in
//! the in-crate `ik` unit tests; this test exists only to prove the public
//! URDF-loading path reaches it.

mod common;

use srs_model::ArmAnglePolicy;

#[test]
fn fixture_round_trip() {
    let arm = common::arm("left");
    let limits = arm.limits();

    // A small deterministic spread of in-limit, non-singular configurations.
    let samples: [[f64; 7]; 4] = [
        [0.3, 0.1, -0.2, 0.6, 0.1, -0.3, 0.2],
        [-0.5, 0.0, 0.4, 1.0, -0.4, 0.5, -0.1],
        [0.1, -0.3, 0.5, 0.8, 0.3, -0.2, 0.4],
        [-0.2, 0.15, -0.3, 1.2, -0.1, 0.4, -0.5],
    ];

    for q in samples {
        for (i, (&v, l)) in q.iter().zip(&limits).enumerate() {
            assert!(
                l.contains(v),
                "seed sample joint {i} = {v} outside [{}, {}]",
                l.lo,
                l.hi
            );
        }
        let target = arm.at(&q).ee_pose();

        let sol = arm
            .solve_ik(&target, ArmAnglePolicy::FromSeed, &q)
            .unwrap_or_else(|| panic!("no IK solution for {q:?}"));
        for (i, (&v, l)) in sol.q.iter().zip(&limits).enumerate() {
            assert!(
                l.contains(v),
                "joint {i} = {v} outside [{}, {}]",
                l.lo,
                l.hi
            );
        }
        let got = arm.at(&sol.q).ee_pose();
        let pos_err = (got.translation.vector - target.translation.vector).norm();
        let rot_err = got.rotation.angle_to(&target.rotation);
        assert!(
            pos_err < 1e-6 && rot_err < 1e-6,
            "round-trip pose error pos={pos_err:.2e} rot={rot_err:.2e} for {q:?}"
        );
    }
}
