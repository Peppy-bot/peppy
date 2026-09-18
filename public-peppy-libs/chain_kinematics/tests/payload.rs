//! What each segment carries beyond its own link, and the proof that nothing is
//! dropped on the way.
//!
//! Absolute values, hand-computed from the URDF, so this fails if the lumping
//! silently drops a body or places one in the wrong frame - which a
//! self-consistency check would not catch.

mod common;
use common::{openarm, so101};

use chain_kinematics::{Chain, ChainSpec, JointSelection, Payload, Tree, segments_carrying};

const OPENARM: &str = include_str!("fixtures/openarm_7dof.urdf");

/// Every gram below the chain's base rides on exactly one segment, so the
/// segment masses have to add up to the robot. The invariant a dynamics layer
/// stands on: a link nobody carries is a link gravity compensation never lifts,
/// and reading one inertial per segment would silently be reading less than the
/// arm.
fn carries_the_whole_arm<const N: usize>(chain: &Chain<N>, base: &str, label: &str) {
    let tree = chain.tree();
    let base = tree.link_index(base).expect("the chain's base link");
    let whole: f64 = tree
        .subtree_from(base, &|_| 0.0)
        .into_iter()
        .map(|(link, _)| tree.link(link).mass)
        .sum();
    let carried: f64 = (0..N).map(|i| chain.at(&[0.0; N]).mass(i)).sum();
    assert!(
        (carried - whole).abs() < 1e-12,
        "{label}: the segments carry {carried} kg of a {whole} kg arm, \
         so {} kg hangs on nothing",
        whole - carried
    );
}

#[test]
fn the_segments_carry_every_gram_of_both_robots() {
    carries_the_whole_arm(&openarm(), "openarm_left_link0", "OpenArm");
    carries_the_whole_arm(&so101(), "base_link", "SO-101");
}

#[test]
fn a_jaw_beside_the_tip_frame_still_rides_on_the_last_joint() {
    // The SO-101's tip is `gripper_frame_link`, a fixed child of `gripper_link`,
    // and the moving jaw hangs off `gripper_link` on a branch beside it rather
    // than below it. Lumping only what sits under the tip would leave the jaw on
    // nothing at all, which is 2.5% of this arm.
    const BARE_GRIPPER: f64 = 0.087;
    const JAW: f64 = 0.012;
    /// The tip link's own nominal inertial. Weightless in practice, but it is
    /// the tip, so a lump taken from below the tip never saw it either.
    const TIP: f64 = 1e-9;
    let chain = so101();
    let carried = chain.at(&[0.0; 5]).mass(4);
    assert!(
        (carried - (BARE_GRIPPER + JAW + TIP)).abs() < 1e-12,
        "the last segment carries {carried} kg, against a bare gripper of {BARE_GRIPPER} kg"
    );
}

#[test]
fn lumps_the_gripper_body_and_fingers_in_the_last_segment_frame() {
    let robot = urdf_rs::read_from_string(OPENARM).expect("parse OpenArm");
    let tree = Tree::from_robot(&robot).expect("build tree");
    let link7 = tree.link_index("openarm_left_link7").expect("link7");
    // link7 is both the last segment's link and the tip, and joint7 is the only
    // actuated joint above the bodies below it.
    let joint7 = tree
        .joints()
        .iter()
        .position(|j| j.name == "openarm_left_joint7")
        .expect("joint7");
    let mut drives = vec![None; tree.joints().len()];
    drives[joint7] = Some(6);
    let base = tree.link_index("openarm_left_link0").expect("link0");
    let rides_on = segments_carrying(&tree, base, &drives);

    let p = Payload::carried_by(&tree, link7, 6, &rides_on);

    // Three bodies hang off link7, read straight from the URDF:
    //   hand          0.127 kg           at (0.000001, 0, 0.101951), fixed at the tip
    //   right finger  0.03602545343277134 kg at (0.0064528, -0.01702, 0.0219685)
    //   left finger   same mass              at (0.0064528, +0.01702, 0.0219685)
    // Both fingers sit on prismatic joints, frozen at zero, offset (0, 0, 0.1025)
    // from the hand - which is why their z in the segment frame is 0.1025 + 0.0219685.
    // The tcp frame is massless.
    const HAND: f64 = 0.127;
    const FINGER: f64 = 0.036_025_453_432_771_34;
    assert!(
        (p.mass - (HAND + 2.0 * FINGER)).abs() < 1e-12,
        "lumped mass = {} kg, expected the hand and two fingers",
        p.mass
    );
    // Mass-weighted average of the three, in the segment frame.
    assert!(
        (p.com.x - 0.002_336_372_635_248_108_6).abs() < 1e-12,
        "com.x = {}",
        p.com.x
    );
    assert!(
        p.com.y.abs() < 1e-12,
        "com.y = {} (the fingers mirror in y, so it must vanish)",
        p.com.y
    );
    assert!(
        (p.com.z - 0.110_101_710_393_099_5).abs() < 1e-12,
        "com.z = {}",
        p.com.z
    );
}

#[test]
fn the_payload_reaches_the_last_segment_of_a_built_chain() {
    // Lumping is only useful if a dynamics layer sees it, which means it has to
    // land in the segment's inertial rather than being computed and dropped.
    let robot = urdf_rs::read_from_string(OPENARM).expect("parse OpenArm");
    let chain = Chain::<7>::from_urdf(
        &robot,
        &ChainSpec {
            base_link: Some("openarm_left_link0"),
            tip_link: "openarm_left_link7",
            joints: JointSelection::PathOrder,
        },
    )
    .expect("build chain");

    // link7's own inertial mass, read straight from the URDF, versus what the
    // chain reports for that segment: the difference is the hand and two fingers.
    let tree = Tree::from_robot(&robot).expect("build tree");
    let bare = tree
        .link(tree.link_index("openarm_left_link7").unwrap())
        .mass;
    let carried = chain.at(&[0.0; 7]).mass(6);
    assert!(
        (carried - bare - 0.199_050_906_865_542_66).abs() < 1e-12,
        "last segment carries {carried} kg against a bare link of {bare} kg"
    );
}
