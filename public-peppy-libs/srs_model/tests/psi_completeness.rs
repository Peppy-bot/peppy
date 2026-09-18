//! Completeness of the IK `FromSeed` redundancy resolution.
//!
//! `Arm::solve_ik` resolves the arm angle (ψ) analytically: it computes the exact
//! set of feasible ψ intervals and returns the one nearest the seed. So whenever
//! a reachable target admits *any* in-limit configuration, it must return one,
//! regardless of the seed.
//!
//! Each target here is the FK of a real in-limit sample, so it is reachable and
//! at least one ψ (the sample's own arm angle) is feasible. Seeding with an
//! *unrelated* configuration forces it off that angle, exercising the interval
//! search rather than the trivial "seed angle already works" path. Deterministic
//! (fixed RNG seed); a regression in the analytic search fails this immediately.

mod common;

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use srs_model::ArmAnglePolicy;

/// Joint-4 floor that keeps targets off the straight-arm singularity, where the
/// arm angle is geometrically undefined and a miss is expected, not a defect.
const ELBOW_FLEX_FLOOR: f64 = 0.05;

#[test]
fn from_seed_solves_every_reachable_target_from_a_bad_seed() {
    for side in ["left", "right"] {
        let arm = common::arm(side);
        let mut rng = StdRng::seed_from_u64(0xBEEF);
        let lim = arm.limits();
        let sample = |rng: &mut StdRng| -> [f64; 7] {
            std::array::from_fn(|i| rng.random_range(lim[i].lo..lim[i].hi))
        };

        let (mut miss, mut solved) = (0u32, 0u32);
        for _ in 0..3000 {
            let q = sample(&mut rng);
            if q[3] < ELBOW_FLEX_FLOOR {
                continue; // straight-arm boundary: arm angle undefined
            }
            solved += 1;
            let target = arm.at(&q).ee_pose();
            // Decorrelated seed: its arm angle is not the target's, so the solver
            // must search the feasible ψ set. The target is reachable (q solves
            // it), so the analytic search is required to return a solution.
            let bad_seed = sample(&mut rng);
            let Some(sol) = arm.solve_ik(&target, ArmAnglePolicy::FromSeed, &bad_seed) else {
                miss += 1;
                continue;
            };
            // And the returned solution must actually reach the target.
            let got = arm.at(&sol.q).ee_pose();
            assert!(
                (got.translation.vector - target.translation.vector).norm() < 1e-6
                    && got.rotation.angle_to(&target.rotation) < 1e-6,
                "{side}: returned solution does not reach the target",
            );
        }
        assert_eq!(
            miss, 0,
            "{side}: analytic ψ search missed {miss}/{solved} reachable targets",
        );
    }
}
