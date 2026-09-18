//! The qualification suite for the point-to-point layer: the same measured bar
//! `chain_kinematics_py` published, ported as tests, so "the engine passes the
//! oracle" is a cargo test verdict rather than a claim.
//!
//! Poses are reachable by construction (forward kinematics of in-limit
//! configurations), seeds are unrelated to the answers, and every draw comes
//! from a fixed generator, so a verdict is a function of the code alone.

mod common;

use chain_kinematics::nalgebra::{Isometry3, Translation3, UnitQuaternion};
use chain_kinematics::{Goal, Kinematics};
use common::{SplitMix64, openarm, so101};

fn kin5() -> Kinematics<5> {
    Kinematics::new(so101())
}

fn kin7() -> Kinematics<7> {
    Kinematics::new(openarm())
}

#[test]
fn reachable_poses_are_never_refused() {
    fn run<const N: usize>(kinematics: &Kinematics<N>, cases: usize, rng: u64, label: &str) {
        let mut rng = SplitMix64(rng);
        let unrelated = kinematics.limits().map(|l| l.midpoint());
        let one_degree = std::f64::consts::PI / 180.0;
        let (mut refused, mut worst_m, mut abandoned, mut worst_angle) =
            (0usize, 0.0f64, 0usize, 0.0f64);
        for _ in 0..cases {
            let target = kinematics.forward_kinematics(&rng.config(kinematics.chain()));
            match kinematics.inverse_kinematics(&unrelated, &Goal::at(target)) {
                None => refused += 1,
                Some(solved) => {
                    let reached = kinematics.forward_kinematics(&solved);
                    worst_m = worst_m
                        .max((reached.translation.vector - target.translation.vector).norm());
                    let angle = reached.rotation.angle_to(&target.rotation);
                    if angle > one_degree {
                        abandoned += 1;
                        worst_angle = worst_angle.max(angle);
                    }
                }
            }
        }
        eprintln!(
            "{label}: refused {refused}/{cases}, worst position {worst_m:.2e} m, \
             orientation abandoned {abandoned}/{cases} (worst {:.1} deg)",
            worst_angle.to_degrees()
        );
        assert_eq!(
            refused, 0,
            "{label}: refused {refused} of {cases} reachable poses"
        );
        assert!(
            worst_m <= 1.0e-3,
            "{label}: an accepted solution missed by {worst_m:.2e} m"
        );
        // The measured bar: 1/1000 abandoned on the 5-DOF arm, 0/300 on the
        // 7-DOF, worst miss 2.1 degrees. The gate carries slack for the debug
        // subset, where one abandoned draw is a larger fraction.
        let allowed = (cases as f64 * 0.003 + 1.0) as usize;
        assert!(
            abandoned <= allowed,
            "{label}: abandoned {abandoned}/{cases} attainable orientations (allowed {allowed})"
        );
        assert!(
            worst_angle <= 15.0_f64.to_radians(),
            "{label}: worst abandoned orientation {:.1} deg",
            worst_angle.to_degrees()
        );
    }
    // Full scale is the qualification run (`cargo test --release`); the debug
    // build keeps a fast regression subset of the same draws.
    let (so101_cases, openarm_cases) = if cfg!(debug_assertions) {
        (150, 60)
    } else {
        (1000, 300)
    };
    run(&kin5(), so101_cases, 11, "SO-101");
    run(&kin7(), openarm_cases, 12, "OpenArm");
}

#[test]
fn track_comes_to_rest_against_an_unreachable_target() {
    // The frozen clause, ported shape and all: seven unreachable directions on
    // both chains, and exactly zero motion over the last 50 of 200 ticks.
    fn run<const N: usize>(kinematics: &Kinematics<N>, label: &str) {
        let directions: [[f64; 3]; 7] = [
            [1.0, 0.0, 0.0],
            [-1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, -1.0, 0.0],
            [0.0, 0.0, 1.0],
            [0.577, 0.577, 0.577],
            [-0.577, 0.577, -0.577],
        ];
        for direction in directions {
            let target = Isometry3::from_parts(
                Translation3::new(
                    direction[0] * 10.0,
                    direction[1] * 10.0,
                    direction[2] * 10.0,
                ),
                UnitQuaternion::identity(),
            );
            let mut q = kinematics.limits().map(|l| l.midpoint());
            let mut trail = Vec::with_capacity(200);
            for _ in 0..200 {
                q = kinematics.track(&q, &target);
                trail.push(q);
            }
            let settled = trail[150];
            for (tick, sample) in trail.iter().enumerate().skip(150) {
                assert_eq!(
                    *sample, settled,
                    "{label}: direction {direction:?} still moving at tick {tick}"
                );
            }
        }
    }
    run(&kin5(), "SO-101");
    run(&kin7(), "OpenArm");
}

#[test]
fn identical_solves_answer_identically() {
    // The determinism contract, with an unrelated solve between the two calls
    // so any hidden state would show.
    let kinematics = kin5();
    let mut rng = SplitMix64(3);
    let goal = Goal::at(kinematics.forward_kinematics(&rng.config(kinematics.chain())));
    let seed = kinematics.limits().map(|l| l.midpoint());
    let first = kinematics.inverse_kinematics(&seed, &goal);
    let unrelated_seed = rng.config(kinematics.chain());
    let unrelated = Goal::at(kinematics.forward_kinematics(&rng.config(kinematics.chain())));
    let _ = kinematics.inverse_kinematics(&unrelated_seed, &unrelated);
    let second = kinematics.inverse_kinematics(&seed, &goal);
    assert_eq!(first, second);
}

#[test]
fn an_unreachable_target_is_refused() {
    let kinematics = kin5();
    let seed = kinematics.limits().map(|l| l.midpoint());
    let far = Isometry3::from_parts(Translation3::new(0.0, 0.0, 5.0), UnitQuaternion::identity());
    assert!(
        kinematics
            .inverse_kinematics(&seed, &Goal::at(far))
            .is_none()
    );
    assert!(kinematics.continue_to(&seed, &Goal::at(far)).is_none());
}

#[test]
fn a_caller_named_position_bar_loosens_acceptance_per_goal() {
    // The same target flips refusal to acceptance on the caller's own bar, with
    // no second solver instance.
    let kinematics = kin5();
    let seed = kinematics.limits().map(|l| l.midpoint());
    let far = Isometry3::from_parts(Translation3::new(0.0, 0.0, 5.0), UnitQuaternion::identity());
    assert!(
        kinematics
            .inverse_kinematics(&seed, &Goal::at(far))
            .is_none()
    );
    let loose = Goal::at(far)
        .position_within(10.0)
        .expect("a positive bar parses");
    let accepted = kinematics
        .inverse_kinematics(&seed, &loose)
        .expect("a 10 m bar accepts the boundary");
    let reached = kinematics.forward_kinematics(&accepted);
    assert!((reached.translation.vector - far.translation.vector).norm() <= 10.0);
}

#[test]
fn tolerances_are_parsed_at_the_boundary() {
    let goal = Goal::at(Isometry3::identity());
    for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert!(
            goal.position_within(bad).is_err(),
            "position bar {bad} must not parse"
        );
        assert!(
            goal.orientation_within(bad).is_err(),
            "orientation bar {bad} must not parse"
        );
    }
}

#[test]
#[should_panic(expected = "finite")]
fn a_non_finite_seed_is_rejected() {
    let kinematics = kin5();
    let mut seed = kinematics.limits().map(|l| l.midpoint());
    seed[1] = f64::NAN;
    let _ = kinematics.track(&seed, &Isometry3::identity());
}

#[test]
fn a_seed_outside_limits_is_clamped_not_refused() {
    let kinematics = kin5();
    let mut rng = SplitMix64(5);
    let target = kinematics.forward_kinematics(&rng.config(kinematics.chain()));
    let solved = kinematics
        .inverse_kinematics(&[1.0e3; 5], &Goal::at(target))
        .expect("a wild seed is clamped, then solved from");
    assert!(
        kinematics
            .limits()
            .iter()
            .zip(solved)
            .all(|(l, v)| l.contains(v)),
        "the answer must be in limits"
    );
}

#[test]
fn continue_to_stays_in_the_seed_neighbourhood() {
    // A path's next sample: solvable from the previous one, and answered with
    // the continuation rather than a distant reconfiguration.
    let kinematics = kin5();
    let limits = kinematics.limits();
    let mut rng = SplitMix64(9);
    let q0 = rng.config(kinematics.chain());
    let q1: [f64; 5] =
        std::array::from_fn(|i| limits[i].clamp(q0[i] + if i % 2 == 0 { 0.05 } else { -0.05 }));
    let target = kinematics.forward_kinematics(&q1);
    let stepped = kinematics
        .continue_to(&q0, &Goal::at(target))
        .expect("the next sample of a path stays solvable from the previous one");
    let drift = stepped
        .iter()
        .zip(q0)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0_f64, f64::max);
    assert!(
        drift < 0.5,
        "continue_to jumped {drift:.2} rad away from its seed"
    );
}
