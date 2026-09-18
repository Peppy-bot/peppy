//! Microbenchmarks for the per-call compute cost of the library entry points.
//! Run with `cargo bench`.
//!
//! Each bench mirrors what a consumer does on a control tick: the `get_fk` /
//! `get_*` dynamics benches re-pose the arm at the requested configuration and
//! evaluate the term; the `get_ik` benches run the closed-form solver. The
//! fixture is the bundled OpenArm V1.0 (left arm).
//!
//! IK has two cost regimes:
//!   - `FromSeed` always computes the exact feasible-arm-angle intervals (the
//!     "cold"/search path); the seed only selects which feasible angle is used,
//!     so a related vs decorrelated seed costs the same (both measured below).
//!   - `Fixed(psi)` skips the interval search (single placement), so it is the
//!     cheaper path.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use srs_model::{Arm, ArmAnglePolicy, JointVec, damped_pseudo_inverse};

const URDF: &str = include_str!("../tests/fixtures/openarm_v10.urdf");
const BASE: &str = "openarm_left_link0";

fn benchmarks(c: &mut Criterion) {
    let arm = Arm::from_urdf(URDF, BASE).expect("load fixture arm");
    // A non-singular configuration and a non-trivial velocity.
    let q: JointVec = [0.3, -0.4, 0.2, 0.8, -0.5, 0.3, 0.6];
    let qd: JointVec = [0.4, -0.3, 0.5, -0.2, 0.6, -0.1, 0.3];

    // IK target: the FK of `q`, so it is reachable. `q` is a "warm" seed (it is
    // the solution); `cold_seed` is decorrelated, exercising the same interval
    // search from a different starting angle. `psi` is `q`'s (feasible) arm angle.
    let target = arm.at(&q).ee_pose();
    let cold_seed: JointVec = [0.0, 0.2, -0.2, 1.2, 0.3, -0.3, 0.4];
    let psi = arm.arm_angle(&q).expect("benchmark seed is non-singular");

    c.bench_function("get_fk", |b| {
        b.iter(|| black_box(arm.at(black_box(&q)).ee_pose()))
    });

    c.bench_function("get_gravity", |b| {
        b.iter(|| black_box(arm.at(black_box(&q)).gravity_torques()))
    });

    c.bench_function("get_coriolis", |b| {
        b.iter(|| black_box(arm.at(black_box(&q)).coriolis_torques(black_box(&qd))))
    });

    // The combined compensation the arm computes every control tick: gravity + Coriolis.
    c.bench_function("get_compensation", |b| {
        b.iter(|| {
            let posed = arm.at(black_box(&q));
            let g = posed.gravity_torques();
            let co = posed.coriolis_torques(black_box(&qd));
            black_box((g, co))
        })
    });

    c.bench_function("get_jacobian", |b| {
        b.iter(|| black_box(arm.at(black_box(&q)).jacobian()))
    });

    // A resolved-rate control tick: Jacobian + damped-least-squares inverse.
    c.bench_function("get_jacobian_dls", |b| {
        b.iter(|| {
            let j = arm.at(black_box(&q)).jacobian();
            black_box(damped_pseudo_inverse(&j, black_box(0.05)))
        })
    });

    // IK, FromSeed with a warm seed (the solution itself).
    c.bench_function("get_ik_from_seed_warm", |b| {
        b.iter(|| {
            black_box(arm.solve_ik(black_box(&target), ArmAnglePolicy::FromSeed, black_box(&q)))
        })
    });

    // IK, FromSeed with a decorrelated ("cold") seed: same interval search.
    c.bench_function("get_ik_from_seed_cold", |b| {
        b.iter(|| {
            black_box(arm.solve_ik(
                black_box(&target),
                ArmAnglePolicy::FromSeed,
                black_box(&cold_seed),
            ))
        })
    });

    // IK, Fixed arm angle: skips the feasible-interval search (cheaper path).
    c.bench_function("get_ik_fixed", |b| {
        b.iter(|| {
            black_box(arm.solve_ik(
                black_box(&target),
                ArmAnglePolicy::Fixed(black_box(psi)),
                black_box(&q),
            ))
        })
    });
}

criterion_group!(benches, benchmarks);
criterion_main!(benches);
