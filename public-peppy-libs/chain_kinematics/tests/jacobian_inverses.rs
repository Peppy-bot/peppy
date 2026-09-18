//! The two inverses and the projector, held to the identities that define them.
//!
//! These are the pieces a redundancy-resolving controller is built from: an
//! exact inverse where the Jacobian has rank to spare, a damped one that stays
//! defined where it does not, and a projector that separates secondary motion
//! from the task. Nothing in this repository calls the exact inverse or the
//! projector yet, which is exactly why they are pinned here: an identity that is
//! never checked is an identity that quietly stops holding.

use chain_kinematics::nalgebra::{DMatrix, SVector};
use chain_kinematics::{
    Chain, damped_pseudo_inverse, manipulability, null_space_projector, try_pseudo_inverse,
};

mod common;
use common::{openarm, sample, so101};

/// Rank tolerance used throughout: well below the smallest singular value of a
/// healthy Jacobian on either robot, well above rounding noise.
const EPS: f64 = 1e-6;

/// Fully extended: shoulder, elbow and wrist collinear, so the Jacobian loses a
/// direction. Reachable on the fixture, whose elbow bottoms out at 0.0; the real
/// arm carries a 0.05 rad floor precisely to stay off it.
const STRAIGHT: [f64; 7] = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];

#[test]
fn the_exact_inverse_realizes_the_twist_it_was_asked_for() {
    // J J⁺ = I on a chain with rank to spare: the joint rates it returns produce
    // exactly the commanded twist, which is the property that makes it "exact".
    let chain = openarm();
    let mut checked = 0;
    for k in 0..24 {
        let q = sample(&chain, k);
        let j = chain.at(&q).jacobian();
        let Some(pinv) = try_pseudo_inverse(&j, EPS) else {
            continue;
        };
        checked += 1;
        let realized = j * pinv;
        for r in 0..6 {
            for c in 0..6 {
                let want = if r == c { 1.0 } else { 0.0 };
                assert!(
                    (realized[(r, c)] - want).abs() < 1e-6,
                    "J J+ is not the identity at sample {k}: [{r}][{c}] = {}",
                    realized[(r, c)]
                );
            }
        }
    }
    // The samples that refuse are skipped, so without this the suite would pass
    // on a `try_pseudo_inverse` that refused everything.
    assert_eq!(
        checked, 24,
        "every sample should be well enough conditioned"
    );
}

#[test]
fn the_exact_inverse_is_the_minimum_norm_solution() {
    // Of the many joint rates realizing a twist on a redundant arm, J⁺ returns
    // the shortest. Anything with a null-space component added is longer.
    let chain = openarm();
    let q = sample(&chain, 3);
    let j = chain.at(&q).jacobian();
    let pinv = try_pseudo_inverse(&j, EPS).expect("sample 3 is well conditioned");
    let twist = SVector::<f64, 6>::from_column_slice(&[0.02, -0.01, 0.03, 0.01, 0.02, -0.01]);
    let minimum = pinv * twist;

    let projector = null_space_projector(&j, EPS);
    for k in 0..8 {
        let arbitrary = SVector::<f64, 7>::from_fn(|i, _| ((i + k) as f64 * 0.37).sin());
        let secondary = projector * arbitrary;
        if secondary.norm() < 1e-9 {
            continue;
        }
        let alternative = minimum + secondary;
        // Both realize the same twist...
        assert!(
            (j * alternative - j * minimum).norm() < 1e-9,
            "the projected term changed the twist"
        );
        // ...and the one without the secondary term is shorter.
        assert!(
            minimum.norm() < alternative.norm(),
            "J+ returned {} but a null-space alternative was shorter at {}",
            minimum.norm(),
            alternative.norm()
        );
    }
}

#[test]
fn the_exact_inverse_refuses_a_singular_jacobian_where_the_damped_one_answers() {
    // The reason both exist. At full extension the exact inverse has nothing
    // honest to return, and says so; the damped one stays finite and bounded,
    // which is what lets a controller drive through the configuration.
    let chain = openarm();
    let j = chain.at(&STRAIGHT).jacobian();
    assert!(
        manipulability(&j) < 1e-9,
        "the straight configuration should be singular, manipulability is {}",
        manipulability(&j)
    );
    assert!(
        try_pseudo_inverse(&j, EPS).is_none(),
        "the exact inverse must refuse a rank-deficient Jacobian"
    );
    let damped = damped_pseudo_inverse(&j, 0.05);
    assert!(
        damped.iter().all(|x| x.is_finite()),
        "the damped inverse must stay finite at a singularity"
    );
    assert!(
        damped.norm() < 1.0e3,
        "the damped inverse must stay bounded at a singularity, got norm {}",
        damped.norm()
    );
}

#[test]
fn the_exact_inverse_refuses_an_under_actuated_chain_it_cannot_invert() {
    // Five joints cannot realize an arbitrary six-dimensional twist, so there is
    // no right inverse to return. The guard is on the smallest singular value,
    // and a 6x5 Jacobian has at most five, so the sixth direction is missing
    // rather than small.
    let chain = so101();
    for k in 0..12 {
        let q = sample(&chain, k);
        let j = chain.at(&q).jacobian();
        assert!(
            try_pseudo_inverse(&j, EPS).is_none(),
            "sample {k}: a 6x5 Jacobian has no right inverse, so this must refuse"
        );
        assert!(
            damped_pseudo_inverse(&j, 0.05)
                .iter()
                .all(|x| x.is_finite()),
            "sample {k}: the damped inverse must still answer"
        );
    }
}

#[test]
fn the_projector_is_a_projector() {
    // P must be idempotent and symmetric, or it is not an orthogonal projection
    // and the "secondary motion" it produces is not confined to the null space.
    fn check<const N: usize>(chain: &Chain<N>, label: &str) {
        for k in 0..12 {
            let q = sample(chain, k);
            let p = null_space_projector(&chain.at(&q).jacobian(), EPS);
            let pp = p * p;
            assert!(
                (pp - p).norm() < 1e-9,
                "{label} sample {k}: P*P differs from P by {}",
                (pp - p).norm()
            );
            assert!(
                (p.transpose() - p).norm() < 1e-9,
                "{label} sample {k}: P is not symmetric"
            );
        }
    }
    check(&openarm(), "OpenArm");
    check(&so101(), "SO-101");
}

#[test]
fn projected_motion_leaves_the_tip_alone() {
    // The property the projector exists for: joint motion inside the null space
    // moves the arm without moving the end effector.
    let chain = openarm();
    for k in 0..12 {
        let q = sample(&chain, k);
        let j = chain.at(&q).jacobian();
        let p = null_space_projector(&j, EPS);
        let arbitrary = SVector::<f64, 7>::from_fn(|i, _| ((i + k) as f64 * 0.61).cos());
        let secondary = p * arbitrary;
        assert!(
            (j * secondary).norm() < 1e-9,
            "sample {k}: null-space motion moved the tip by {}",
            (j * secondary).norm()
        );
    }
}

#[test]
fn the_null_space_is_as_large_as_the_redundancy() {
    // An orthogonal projector's trace is the dimension it projects onto. A
    // 7-DOF arm meeting a 6-D task has exactly one spare direction; the 5-DOF
    // arm has none, because every joint is already spoken for.
    let openarm = openarm();
    let q = sample(&openarm, 5);
    let trace = null_space_projector(&openarm.at(&q).jacobian(), EPS).trace();
    assert!(
        (trace - 1.0).abs() < 1e-6,
        "a 7-DOF arm on a 6-D task should have a one-dimensional null space, trace is {trace}"
    );

    let so101 = so101();
    let q = sample(&so101, 5);
    let trace = null_space_projector(&so101.at(&q).jacobian(), EPS).trace();
    assert!(
        trace.abs() < 1e-6,
        "a 5-DOF arm has no spare direction, yet the projector's trace is {trace}"
    );
}

#[test]
fn the_null_space_grows_at_a_singularity() {
    // Losing a task direction hands that direction to the null space, so the
    // projector stays exact where the exact inverse cannot be formed at all.
    let chain = openarm();
    let regular = null_space_projector(&chain.at(&sample(&chain, 5)).jacobian(), EPS).trace();
    let singular = null_space_projector(&chain.at(&STRAIGHT).jacobian(), EPS).trace();
    assert!(
        singular > regular + 0.5,
        "the null space should grow at a singularity: {regular} regular vs {singular} singular"
    );
}

#[test]
fn manipulability_is_the_product_of_the_singular_values_at_any_joint_count() {
    // The index is a distance to a singularity, and a chain of fewer than six
    // joints is not standing in one just for being short. Read off `J Jᵀ` alone
    // it would be: that product is 6x6 of rank at most N, so its determinant is
    // zero at every posture a five-joint arm can hold.
    fn check<const N: usize>(chain: &Chain<N>, label: &str) {
        for k in 0..24 {
            let q = sample(chain, k);
            let j = chain.at(&q).jacobian();
            let singular = DMatrix::from_iterator(6, N, j.iter().copied())
                .svd(false, false)
                .singular_values;
            let want: f64 = singular.iter().product();
            let got = manipulability(&j);
            assert!(
                got > 0.0,
                "{label} sample {k} is a healthy posture, but reads as singular"
            );
            assert!(
                (got - want).abs() <= 1e-9 * want.max(1.0),
                "{label} sample {k}: manipulability is {got}, the singular values multiply to {want}"
            );
        }
    }
    check(&so101(), "SO-101");
    check(&openarm(), "OpenArm");
}
