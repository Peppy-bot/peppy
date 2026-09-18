//! Feedforward dynamics against closed forms: fixtures small enough that the
//! right answer is a textbook expression, not another implementation.

mod common;

use chain_kinematics::{Chain, ChainSpec, JointSelection};
use common::{SplitMix64, openarm};

const PLANAR: &str = r#"<?xml version="1.0"?><robot name="polar">
  <link name="base"/>
  <link name="boom"><inertial><mass value="0.0"/><inertia ixx="0" ixy="0" ixz="0" iyy="0" iyz="0" izz="0"/></inertial></link>
  <link name="slider"><inertial><origin xyz="0 0 0"/><mass value="0.7"/><inertia ixx="0" ixy="0" ixz="0" iyy="0" iyz="0" izz="0"/></inertial></link>
  <joint name="swing" type="revolute">
    <parent link="base"/><child link="boom"/><axis xyz="0 0 1"/>
    <limit lower="-3.0" upper="3.0" effort="1" velocity="1"/>
  </joint>
  <joint name="slide" type="prismatic">
    <parent link="boom"/><child link="slider"/><axis xyz="1 0 0"/>
    <limit lower="0.1" upper="1.5" effort="1" velocity="1"/>
  </joint>
</robot>"#;

/// A point mass sliding along a rotating boom: the polar-coordinates classic.
fn polar() -> Chain<2> {
    let robot = urdf_rs::read_from_string(PLANAR).expect("parse polar fixture");
    Chain::<2>::from_urdf(
        &robot,
        &ChainSpec {
            base_link: None,
            tip_link: "slider",
            joints: JointSelection::PathOrder,
        },
    )
    .expect("polar fixture is a two-joint chain")
}

const VERTICAL_SLIDER: &str = r#"<?xml version="1.0"?><robot name="lift">
  <link name="base"/>
  <link name="carriage"><inertial><mass value="1.3"/><inertia ixx="0" ixy="0" ixz="0" iyy="0" iyz="0" izz="0"/></inertial></link>
  <joint name="lift" type="prismatic">
    <parent link="base"/><child link="carriage"/><axis xyz="0 0 1"/>
    <limit lower="0.0" upper="1.0" effort="1" velocity="1"/>
  </joint>
</robot>"#;

#[test]
fn a_vertical_slider_holds_its_own_weight() {
    let robot = urdf_rs::read_from_string(VERTICAL_SLIDER).expect("parse lift fixture");
    let chain = Chain::<1>::from_urdf(
        &robot,
        &ChainSpec {
            base_link: None,
            tip_link: "carriage",
            joints: JointSelection::PathOrder,
        },
    )
    .expect("lift fixture is a one-joint chain");
    let tau = chain.at(&[0.4]).gravity_torques();
    // Holding still against gravity takes exactly the weight, at any height.
    assert!((tau[0] - 1.3 * 9.81).abs() < 1e-12, "tau = {}", tau[0]);
}

#[test]
fn the_polar_slider_matches_the_textbook_coriolis() {
    // Point mass m on a horizontal rotating boom (swing rate w) sliding at
    // rate rdot, mass at radius r. With qddot = 0 the velocity coupling is
    //   swing torque: 2 m r rdot w      (Coriolis)
    //   slide force: -m r w^2           (centripetal)
    let chain = polar();
    let (m, r, w, rdot) = (0.7, 0.9, 1.7, 0.35);
    let tau = chain.at(&[0.3, r]).coriolis_torques(&[w, rdot]);
    let expected_swing = 2.0 * m * r * rdot * w;
    let expected_slide = -m * r * w * w;
    assert!(
        (tau[0] - expected_swing).abs() < 1e-12,
        "swing: got {} expected {expected_swing}",
        tau[0]
    );
    assert!(
        (tau[1] - expected_slide).abs() < 1e-12,
        "slide: got {} expected {expected_slide}",
        tau[1]
    );
    // And the boom's gravity torque is zero: the mass moves in a horizontal
    // plane, while the slide axis is horizontal too.
    let grav = chain.at(&[0.3, r]).gravity_torques();
    assert!(grav[0].abs() < 1e-12 && grav[1].abs() < 1e-12, "{grav:?}");
}

#[test]
fn gravity_is_the_potential_energy_gradient_on_a_real_arm() {
    // The property that defines gravity torque, checked on the 7-DOF fixture
    // by central difference of U = sum(m_i * g * z_com_i).
    let chain = openarm();
    let mut rng = SplitMix64(21);
    for _ in 0..5 {
        let q = rng.config(&chain);
        let tau = chain.at(&q).gravity_torques();
        let h = 1e-6;
        for i in 0..7 {
            let (mut qp, mut qm) = (q, q);
            qp[i] += h;
            qm[i] -= h;
            let u = |q: &[f64; 7]| -> f64 {
                let posed = chain.at(q);
                (0..7)
                    .map(|s| posed.mass(s) * 9.81 * posed.com_world(s).z)
                    .sum()
            };
            let grad = (u(&qp) - u(&qm)) / (2.0 * h);
            assert!(
                (tau[i] - grad).abs() < 1e-3,
                "joint {i}: tau {} vs gradient {grad}",
                tau[i]
            );
        }
    }
}

#[test]
fn zero_velocity_couples_no_torque() {
    let chain = openarm();
    let tau = chain
        .at(&[0.2, -0.4, 0.3, 0.9, -0.2, 0.5, 0.1])
        .coriolis_torques(&[0.0; 7]);
    assert_eq!(tau, [0.0; 7]);
}

/// The OpenArm's seven joints named out of order, so `q` runs in an order the
/// linkage does not. `JointSelection::Named` exists to let a caller keep its wire
/// order, which nothing guarantees matches the URDF's path order.
const SCRAMBLED: [&str; 7] = [
    "openarm_left_joint4",
    "openarm_left_joint1",
    "openarm_left_joint7",
    "openarm_left_joint2",
    "openarm_left_joint6",
    "openarm_left_joint3",
    "openarm_left_joint5",
];

/// Where each entry of [`SCRAMBLED`] sits in the path-ordered chain's `q`.
const FROM_PATH_ORDER: [usize; 7] = [3, 0, 6, 1, 5, 2, 4];

fn scrambled_openarm() -> Chain<7> {
    let robot = urdf_rs::read_from_string(common::OPENARM).expect("parse OpenArm");
    Chain::<7>::from_urdf(
        &robot,
        &ChainSpec {
            base_link: Some("openarm_left_link0"),
            tip_link: "openarm_left_link7",
            joints: JointSelection::Named(&SCRAMBLED),
        },
    )
    .expect("the same seven joints, named in another order")
}

#[test]
fn the_torques_follow_the_chain_and_not_the_order_of_q() {
    // Both recursions walk proximal to distal, which is the order of the linkage
    // and not of `q`. Naming the same seven joints in another order must permute
    // the torques and change nothing else: it is the same arm at the same pose.
    let (path_order, scrambled) = (openarm(), scrambled_openarm());
    let mut draws = SplitMix64(0x5EED_0DD5);
    for k in 0..64 {
        let q = draws.config(&path_order);
        let qdot: [f64; 7] = std::array::from_fn(|_| draws.unit() * 2.0 - 1.0);
        let permuted: [f64; 7] = std::array::from_fn(|i| q[FROM_PATH_ORDER[i]]);
        let permuted_dot: [f64; 7] = std::array::from_fn(|i| qdot[FROM_PATH_ORDER[i]]);

        let want = path_order.at(&q);
        let got = scrambled.at(&permuted);
        assert!(
            (want.ee_pose().translation.vector - got.ee_pose().translation.vector).norm() < 1e-12,
            "draw {k}: the two orders do not pose the same arm"
        );

        let (want_g, got_g) = (want.gravity_torques(), got.gravity_torques());
        let (want_c, got_c) = (
            want.coriolis_torques(&qdot),
            got.coriolis_torques(&permuted_dot),
        );
        for i in 0..7 {
            let j = FROM_PATH_ORDER[i];
            assert!(
                (want_g[j] - got_g[i]).abs() < 1e-9,
                "draw {k}: gravity on '{}' is {} in path order and {} scrambled",
                SCRAMBLED[i],
                want_g[j],
                got_g[i]
            );
            assert!(
                (want_c[j] - got_c[i]).abs() < 1e-9,
                "draw {k}: Coriolis on '{}' is {} in path order and {} scrambled",
                SCRAMBLED[i],
                want_c[j],
                got_c[i]
            );
        }
    }
}

#[test]
fn the_joints_that_move_a_point_are_the_ones_above_it_on_the_chain() {
    // A witness point on a segment is moved by the joints proximal to it and by
    // no others. "Proximal" is a fact about the linkage, so a scrambled `q` must
    // report the same per-joint contributions, at whatever entries it puts them,
    // and zero the same joints.
    use chain_kinematics::nalgebra::Point3;

    let (path_order, scrambled) = (openarm(), scrambled_openarm());
    let q = [0.3, -0.4, 0.2, 0.8, -0.5, 0.3, 0.6];
    let permuted: [f64; 7] = std::array::from_fn(|i| q[FROM_PATH_ORDER[i]]);
    let (posed, posed_scrambled) = (path_order.at(&q), scrambled.at(&permuted));

    for segment in 0..7 {
        let point = Point3::from(posed.link_pose_world(segment).translation.vector);
        // The same segment, at the entry of `q` the scrambled chain gives it.
        let as_named = FROM_PATH_ORDER
            .iter()
            .position(|&j| j == segment)
            .expect("every path-order joint is named");
        let want = posed.point_world_jacobian(&point, segment);
        let got = posed_scrambled.point_world_jacobian(&point, as_named);
        for (i, column) in got.iter().enumerate() {
            let j = FROM_PATH_ORDER[i];
            assert!(
                (want[j] - column).norm() < 1e-12,
                "segment {segment}: '{}' contributes {:?} in path order and {column:?} scrambled",
                SCRAMBLED[i],
                want[j]
            );
            assert!(
                j <= segment || column.norm() == 0.0,
                "segment {segment}: '{}' is below the point and still moves it: {column:?}",
                SCRAMBLED[i]
            );
        }
    }
}
