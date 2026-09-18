//! Feedforward dynamics on the real arm, validated two independent ways: KDL
//! `TreeIdSolver_RNE` reference values (tree inverse dynamics over the whole
//! URDF with the gripper fingers at home, so the folded distal payload is
//! included), and the potential-energy gradient, which is what gravity torque
//! is. The computation itself lives in `chain_kinematics`; this suite is the
//! evidence it holds on a 7-DOF arm, mirror sides included.
//!
//! Tolerance 1e-3 Nm; sub-threshold reference values written 0.0. Regenerate
//! the tables with `tools/kdl_reference.cpp`.

use srs_model::{ARM_DOF, Arm, JointVec};
use std::f64::consts::FRAC_PI_2;

const FIXTURE: &str = include_str!("fixtures/openarm_v10.urdf");
const GRAVITY_MAGNITUDE: f64 = 9.81;

fn arm(side: &str) -> Arm {
    Arm::from_urdf(FIXTURE, &format!("openarm_{side}_link0")).expect("load fixture")
}

const GRAVITY_POSTURES: [JointVec; 4] = [
    [0.0; ARM_DOF],
    [FRAC_PI_2, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    [0.0, 0.0, 0.0, FRAC_PI_2, 0.0, 0.0, 0.0],
    [0.1, -0.2, 0.3, -0.4, 0.5, -0.6, 0.7],
];

fn assert_gravity_matches_kdl(side: &str, expected: [JointVec; 4]) {
    let arm = arm(side);
    for (q, exp) in GRAVITY_POSTURES.iter().zip(&expected) {
        let tau = arm.at(q).gravity_torques();
        for i in 0..ARM_DOF {
            assert!(
                (tau[i] - exp[i]).abs() < 1e-3,
                "{side} arm, q={q:?}, joint {i}: actual={} expected={}",
                tau[i],
                exp[i],
            );
        }
    }
}

#[test]
fn gravity_left_matches_kdl() {
    assert_gravity_matches_kdl(
        "left",
        [
            [0.0983, -0.0515, 0.0, -0.0299, 0.0, 0.0594, -0.0049],
            [10.4090, 0.0, -0.0515, -3.7841, -0.0648, 0.0, 0.4058],
            [-3.7157, -0.0515, 0.0, 3.7841, 0.0648, 0.0, -0.4058],
            [2.6572, -2.4663, 0.2762, -2.1801, 0.0351, -0.1392, 0.3296],
        ],
    );
}

#[test]
fn gravity_right_matches_kdl() {
    assert_gravity_matches_kdl(
        "right",
        [
            [-0.0983, 0.0780, 0.0, -0.0299, 0.0, -0.0594, 0.0049],
            [10.4090, 0.0, -0.0781, 3.7841, -0.0648, 0.0, 0.4058],
            [3.7157, 0.0780, 0.0, 3.7841, -0.0648, 0.0, 0.4058],
            [-0.4429, -2.0486, 0.2803, -1.1611, 0.0865, -0.1971, 0.0782],
        ],
    );
}

#[test]
fn gravity_matches_potential_energy_gradient_both_sides() {
    for side in ["left", "right"] {
        let arm = arm(side);
        for q in [
            [0.0; ARM_DOF],
            [0.1, -0.2, 0.3, -0.4, 0.5, -0.6, 0.7],
            [-0.5, 0.3, -0.2, 0.8, 0.1, 0.4, -0.3],
        ] {
            let potential = |q: &JointVec| -> f64 {
                let posed = arm.at(q);
                (0..ARM_DOF)
                    .map(|i| posed.mass(i) * GRAVITY_MAGNITUDE * posed.com_world(i).z)
                    .sum()
            };
            let tau = arm.at(&q).gravity_torques();
            let h = 1e-6;
            for i in 0..ARM_DOF {
                let (mut qp, mut qm) = (q, q);
                qp[i] += h;
                qm[i] -= h;
                let grad = (potential(&qp) - potential(&qm)) / (2.0 * h);
                assert!(
                    (tau[i] - grad).abs() < 1e-3,
                    "{side} j{i}: tau={} grad={grad}",
                    tau[i],
                );
            }
        }
    }
}

const CORIOLIS_CASES: [(JointVec, JointVec); 4] = [
    ([0.0; ARM_DOF], [5.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
    ([0.0; ARM_DOF], [0.0, 0.0, 0.0, 5.0, 0.0, 0.0, 0.0]),
    (
        [0.0, 0.0, 0.0, FRAC_PI_2, 0.0, 0.0, 0.0],
        [3.0, 0.0, 0.0, 3.0, 0.0, 0.0, 0.0],
    ),
    (
        [0.1, -0.2, 0.3, -0.4, 0.5, -0.6, 0.7],
        [1.0, -1.5, 2.0, -2.5, 3.0, -3.5, 4.0],
    ),
];

fn assert_coriolis_matches_kdl(side: &str, expected: [JointVec; 4]) {
    let arm = arm(side);
    for ((q, qdot), exp) in CORIOLIS_CASES.iter().zip(&expected) {
        let tau = arm.at(q).coriolis_torques(qdot);
        for i in 0..ARM_DOF {
            assert!(
                (tau[i] - exp[i]).abs() < 1e-3,
                "{side} arm, q={q:?} qd={qdot:?}, joint {i}: actual={} expected={}",
                tau[i],
                exp[i],
            );
        }
    }
}

#[test]
fn coriolis_zero_velocity_gives_zero_torque() {
    let tau = arm("left")
        .at(&[0.0; ARM_DOF])
        .coriolis_torques(&[0.0; ARM_DOF]);
    assert_eq!(tau, [0.0; ARM_DOF]);
}

#[test]
fn coriolis_left_matches_kdl() {
    assert_coriolis_matches_kdl(
        "left",
        [
            [0.0, -0.0714, 0.0, -0.0168, 0.0, 0.0741, -0.0054],
            [-0.0168, -0.0426, 0.0, 0.0, 0.0, 0.0408, -0.0027],
            [-0.7638, -0.0104, 0.0, 0.7638, 0.0131, 0.0, -0.0819],
            [-2.4372, -1.0816, 0.4952, 0.5812, 0.1273, 0.0689, 0.2063],
        ],
    );
}

#[test]
fn coriolis_right_matches_kdl() {
    assert_coriolis_matches_kdl(
        "right",
        [
            [0.0, 0.0867, 0.0, -0.0168, 0.0, -0.0740, 0.0054],
            [0.0168, 0.0430, 0.0, 0.0, 0.0, -0.0407, 0.0027],
            [-2.2913, 0.0157, 0.0618, 0.7638, -0.0130, -0.0586, 0.0858],
            [0.1017, -0.8665, 0.1347, -0.2242, -0.0224, -0.0181, 0.0163],
        ],
    );
}
