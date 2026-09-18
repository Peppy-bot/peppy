//! The point of this crate: chains that are not one particular arm.
//!
//! Two real robots with different joint counts, different topologies and
//! different vendors, loaded and driven through the same API. The SO-101 is
//! five actuated joints with a gripper branching off the last link; the OpenArm
//! is seven with a two-finger gripper past the wrist. Neither is special-cased.

use chain_kinematics::nalgebra::{Isometry3, Translation3, UnitQuaternion, Vector3};
use chain_kinematics::{
    Chain, ChainSpec, EeCaps, JointSelection, NoSmoothing, ServoLimits, ServoState, ServoStep,
    ServoTolerances, TRACKING_FLOOR_M, ToleranceError, damped_pseudo_inverse, manipulability,
    rate_step_toward,
};

mod common;
use common::{SO101, SO101_JOINTS, openarm, sample, so101};

#[test]
fn loads_arms_of_different_joint_counts() {
    let five = so101();
    let seven = openarm();
    assert_eq!(five.limits().len(), 5);
    assert_eq!(seven.limits().len(), 7);
    for (i, l) in five.limits().iter().enumerate() {
        assert!(l.lo < l.hi, "SO-101 joint {i} has an empty range");
    }
    for (i, l) in seven.limits().iter().enumerate() {
        assert!(l.lo < l.hi, "OpenArm joint {i} has an empty range");
    }
}

/// The strongest check that forward kinematics and the Jacobian agree: every
/// column against a central finite difference of the pose, on both robots.
#[test]
fn jacobian_matches_finite_difference() {
    fn check<const N: usize>(chain: &Chain<N>, label: &str) {
        let h = 1e-7;
        for k in 0..24 {
            let q = sample(chain, k);
            let j = chain.at(&q).jacobian();
            for i in 0..N {
                let (mut qp, mut qm) = (q, q);
                qp[i] += h;
                qm[i] -= h;
                let (a, b) = (chain.at(&qp).ee_pose(), chain.at(&qm).ee_pose());
                let lin = (a.translation.vector - b.translation.vector) / (2.0 * h);
                let ang = (a.rotation * b.rotation.inverse()).scaled_axis() / (2.0 * h);
                for (row, want) in [lin.x, lin.y, lin.z, ang.x, ang.y, ang.z]
                    .into_iter()
                    .enumerate()
                {
                    let got = j[(row, i)];
                    assert!(
                        (got - want).abs() < 1e-5,
                        "{label} column {i} row {row}: {got} vs finite difference {want}"
                    );
                }
            }
        }
    }
    check(&so101(), "SO-101");
    check(&openarm(), "OpenArm");
}

/// Naming the joints fixes the order of `q`, which is the whole reason the order
/// is an input: it is usually the robot's wire order, which no URDF knows.
#[test]
fn named_selection_fixes_the_joint_order() {
    let robot = urdf_rs::read_from_string(SO101).expect("parse SO-101");
    let spec = |names: &'static [&'static str]| ChainSpec {
        base_link: None,
        tip_link: "gripper_frame_link",
        joints: JointSelection::Named(names),
    };
    let forward = Chain::<5>::from_urdf(&robot, &spec(&SO101_JOINTS)).unwrap();
    const REVERSED: [&str; 5] = [
        "wrist_roll",
        "wrist_flex",
        "elbow_flex",
        "shoulder_lift",
        "shoulder_pan",
    ];
    let reversed = Chain::<5>::from_urdf(&robot, &spec(&REVERSED)).unwrap();

    let q = [0.1, -0.2, 0.3, -0.4, 0.5];
    let mut flipped = q;
    flipped.reverse();
    let a = forward.at(&q).ee_pose();
    let b = reversed.at(&flipped).ee_pose();
    assert!(
        (a.translation.vector - b.translation.vector).norm() < 1e-12
            && a.rotation.angle_to(&b.rotation) < 1e-12,
        "reversing the names and the values must describe the same pose"
    );
    // And path order agrees with the names given in path order.
    let path_order = Chain::<5>::from_urdf(
        &robot,
        &ChainSpec {
            base_link: None,
            tip_link: "gripper_frame_link",
            joints: JointSelection::PathOrder,
        },
    )
    .unwrap();
    let c = path_order.at(&q).ee_pose();
    assert!((a.translation.vector - c.translation.vector).norm() < 1e-12);
}

#[test]
fn a_wrong_joint_count_is_refused_at_load() {
    let robot = urdf_rs::read_from_string(SO101).expect("parse SO-101");
    let spec = ChainSpec {
        base_link: None,
        tip_link: "gripper_frame_link",
        joints: JointSelection::PathOrder,
    };
    // The SO-101 arm has five actuated joints; asking for seven must fail here,
    // at load, rather than producing a chain that silently ignores `q[5..]`.
    let err = match Chain::<7>::from_urdf(&robot, &spec) {
        Ok(_) => panic!("a five-joint arm must not load as a seven-joint chain"),
        Err(e) => e,
    };
    assert!(
        format!("{err}").contains("expected 7"),
        "error should name the mismatch: {err}"
    );
}

#[test]
fn the_damped_inverse_serves_an_under_actuated_chain() {
    // Five joints cannot meet an arbitrary six-dimensional twist. The damped
    // inverse must still return a bounded least-squares answer rather than
    // failing, which is what lets one control law drive both robots.
    let chain = so101();
    for k in 0..16 {
        let q = sample(&chain, k);
        let j = chain.at(&q).jacobian();
        let pinv = damped_pseudo_inverse(&j, 0.05);
        assert!(
            pinv.iter().all(|x| x.is_finite()),
            "damped inverse must stay finite on a 6x5 Jacobian"
        );
        assert!(manipulability(&j).is_finite());
    }
}

/// The servo law is the crate's reason to exist, so it has to drive a robot it
/// was not written for.
#[test]
fn the_servo_law_converges_on_both_robots() {
    fn run<const N: usize>(chain: &Chain<N>, seed: [f64; N], label: &str) {
        let limits = ServoLimits {
            max_joint_velocity: [2.0; N],
            ee: EeCaps {
                linear_m_s: 0.15,
                angular_rad_s: 1.0,
            },
            tolerances: ServoTolerances::new(1e-3, 1e-2).expect("a reachable tolerance"),
            dt_s: 0.01,
        };

        // A goal the chain provably reaches: the pose of another configuration.
        let goal_q: [f64; N] = std::array::from_fn(|i| seed[i] * 0.4);
        let start = chain.world_pose(&chain.at(&seed).ee_pose());
        let goal = chain.world_pose(&chain.at(&goal_q).ee_pose());

        let state = ServoState::new(start, goal, 0.05, NoSmoothing);
        let took = chain_kinematics::rollout(chain, state, seed, &limits, 30.0);
        let took = took.unwrap_or_else(|| panic!("{label}: servo did not converge"));
        assert!(took > 0.0 && took < 30.0, "{label}: converged in {took}s");
    }
    run(&so101(), [0.3, -0.4, 0.5, -0.3, 0.2], "SO-101");
    run(&openarm(), [0.3, -0.4, 0.2, 0.8, -0.5, 0.3, 0.6], "OpenArm");
}

#[test]
fn a_held_orientation_is_held_exactly_along_the_whole_line() {
    // A pure-translation move interpolates between two orientations that are the
    // same one, where slerp divides one vanishing sine by another. It stays inside
    // any tolerance, but it is not the identity, so over a move's length the held
    // orientation walks. Taking the endpoint there is what pins it.
    let hold = UnitQuaternion::from_axis_angle(&Vector3::x_axis(), 0.3);
    let at = |x: f64| Isometry3::from_parts(Translation3::new(x, 0.0, 0.0), hold);
    let (from, to) = (at(0.0), at(0.25));
    for k in 0..=20 {
        let s = k as f64 / 20.0;
        let held = chain_kinematics::interpolate(&from, &to, s).rotation;
        assert_eq!(
            held,
            hold,
            "orientation drifted at s = {s}: {:e} rad",
            held.angle_to(&hold)
        );
    }
}

#[test]
fn a_tool_frame_moves_the_end_effector_and_not_the_tip() {
    let bare = openarm();
    let tooled = openarm()
        .with_tool_link("openarm_left_tcp")
        .expect("the OpenArm URDF carries a tcp frame");
    let q = [0.3, -0.4, 0.2, 0.8, -0.5, 0.3, 0.6];
    assert_eq!(bare.at(&q).tip_pose(), tooled.at(&q).tip_pose());
    let tool = tooled.tool();
    assert!(
        tool.translation.vector.norm() > 0.1,
        "tcp should sit off the tip"
    );
    let expected = bare.at(&q).tip_pose() * tool;
    let ee = tooled.at(&q).ee_pose();
    assert!((ee.translation.vector - expected.translation.vector).norm() < 1e-12);
}

#[test]
fn a_tool_reached_through_a_moving_joint_is_refused() {
    // The moving jaw's offset tracks the gripper opening, so it is not a frame
    // anything can be commanded at.
    let err = so101()
        .with_tool_link("moving_jaw_so101_v1_link")
        .expect_err("a link below a revolute joint is not a fixed tool frame");
    assert!(format!("{err}").contains("fixed below the tip"), "{err}");
}

#[test]
fn poses_are_reported_in_the_named_base_frame() {
    // The OpenArm chain names a base partway down the URDF, so the world -> base
    // transform is not the identity, and a pose taken to world and back is the
    // one it started as.
    let root_based = openarm();
    assert_ne!(root_based.base_from_world(), Isometry3::identity());
    let q = [0.2; 7];
    let world = root_based.world_pose(&root_based.at(&q).ee_pose());
    let back = root_based.base_pose(&world);
    let ee = root_based.at(&q).ee_pose();
    assert!((back.translation.vector - ee.translation.vector).norm() < 1e-12);
    assert!(world.translation.vector != Vector3::zeros());
}

#[test]
fn an_arrival_tolerance_the_law_would_never_reach_is_refused() {
    // A step stops correcting position inside the law's tracking floor, so a move
    // asked to arrive tighter than that could only spend its whole budget and
    // time out. The floor itself is reachable and is the tightest that is, which
    // `the_servo_law_converges_on_both_robots` runs at on both robots.
    ServoTolerances::new(TRACKING_FLOOR_M, 1e-2)
        .expect("the floor is the tightest arrival the law can reach");
    for position_m in [
        TRACKING_FLOOR_M * 0.999,
        0.0,
        -1e-3,
        f64::NAN,
        f64::INFINITY,
    ] {
        assert!(
            matches!(
                ServoTolerances::new(position_m, 1e-2),
                Err(ToleranceError::Position(_))
            ),
            "{position_m} m is not an arrival the law can reach"
        );
    }
    for orientation_rad in [0.0, -1e-2, f64::NAN, f64::INFINITY] {
        assert!(
            matches!(
                ServoTolerances::new(1e-2, orientation_rad),
                Err(ToleranceError::Orientation(_))
            ),
            "{orientation_rad} rad is not a finite positive angle"
        );
    }
}

#[test]
fn a_step_stops_correcting_position_inside_the_tracking_floor() {
    // The mechanism the refusal above stands on: within the floor the position
    // term is dropped, so a target that close draws no motion at all and the
    // error stops shrinking. No budget closes a gap nothing is correcting.
    let chain = so101();
    let q = [0.3, -0.4, 0.5, -0.3, 0.2];
    let ee = chain.world_pose(&chain.at(&q).ee_pose());
    let limits = ServoLimits {
        max_joint_velocity: [2.0; 5],
        ee: EeCaps {
            linear_m_s: 0.15,
            angular_rad_s: 1.0,
        },
        tolerances: ServoTolerances::new(TRACKING_FLOOR_M, 1e-2).expect("the floor is reachable"),
        dt_s: 0.01,
    };
    let inside = Isometry3::from_parts(
        Translation3::from(ee.translation.vector + Vector3::new(0.9 * TRACKING_FLOOR_M, 0.0, 0.0)),
        ee.rotation,
    );
    assert_eq!(
        rate_step_toward(&chain, &q, &ee, &inside, &limits),
        q,
        "a target inside the floor should draw no step"
    );

    let outside = Isometry3::from_parts(
        Translation3::from(ee.translation.vector + Vector3::new(1.1 * TRACKING_FLOOR_M, 0.0, 0.0)),
        ee.rotation,
    );
    assert_ne!(
        rate_step_toward(&chain, &q, &ee, &outside, &limits),
        q,
        "a target outside the floor should draw one"
    );
}

/// One revolute joint about z at the origin, reaching a quarter metre out along
/// x to its tip, which the joint therefore sweeps through y. Every coordinate is
/// a dyadic rational, so the tip pose at `q = 0` is exact and an offset from it
/// in y is exactly the number written down: the only way to put a pose error
/// *on* the tracking floor rather than a rounding step off it.
const DYADIC_ARM: &str = r#"<?xml version="1.0"?><robot name="dyadic">
  <link name="base"/>
  <link name="arm"/>
  <link name="tip"/>
  <joint name="spin" type="revolute">
    <parent link="base"/><child link="arm"/><axis xyz="0 0 1"/>
    <origin xyz="0 0 0"/>
    <limit lower="-2.0" upper="2.0" effort="1" velocity="1"/>
  </joint>
  <joint name="reach" type="fixed">
    <parent link="arm"/><child link="tip"/><origin xyz="0.25 0 0"/>
  </joint>
</robot>"#;

#[test]
fn a_pose_error_of_exactly_the_tracking_floor_is_still_corrected() {
    // The floor is the tightest arrival a caller may ask for, and arrival is a
    // strict comparison. So the set the step stops correcting in has to sit
    // strictly inside the floor: an error of exactly the floor that drew no step
    // would be neither corrected nor converged, and the move would spend its
    // whole budget sitting on it.
    let robot = urdf_rs::read_from_string(DYADIC_ARM).expect("parse the dyadic arm");
    let chain = Chain::<1>::from_urdf(
        &robot,
        &ChainSpec {
            base_link: None,
            tip_link: "tip",
            joints: JointSelection::PathOrder,
        },
    )
    .expect("one revolute joint");

    let q = [0.0];
    let ee = chain.world_pose(&chain.at(&q).ee_pose());
    assert_eq!(
        ee.translation.vector,
        Vector3::new(0.25, 0.0, 0.0),
        "the fixture's tip must be exact for this test to sit on the floor"
    );
    let target = Isometry3::from_parts(Translation3::new(0.25, TRACKING_FLOOR_M, 0.0), ee.rotation);
    assert_eq!(
        (target.translation.vector - ee.translation.vector).norm(),
        TRACKING_FLOOR_M,
        "the error must be exactly the floor, not a rounding step past it"
    );

    let limits = ServoLimits {
        max_joint_velocity: [2.0; 1],
        ee: EeCaps {
            linear_m_s: 0.15,
            angular_rad_s: 1.0,
        },
        tolerances: ServoTolerances::new(TRACKING_FLOOR_M, 1e-2).expect("the floor is reachable"),
        dt_s: 0.01,
    };
    assert_ne!(
        rate_step_toward(&chain, &q, &ee, &target, &limits),
        q,
        "an error of exactly the floor must still draw a step"
    );

    let mut state = ServoState::new(ee, target, 0.05, NoSmoothing);
    let mut walked = q;
    let converged = (0..2000).any(|_| match state.step(&chain, &walked, &limits) {
        ServoStep::Stepped(next) => {
            walked = next;
            false
        }
        ServoStep::Converged(_) => true,
    });
    assert!(
        converged,
        "a move of exactly the floor must arrive at the floor tolerance"
    );
}
