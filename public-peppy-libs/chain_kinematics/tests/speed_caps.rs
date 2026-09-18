//! The end-effector speed budget, held to at the tip rather than in the maths.
//!
//! The cap is what stands between a large task error and a fast robot, so it is
//! checked the way it matters: by taking a step, running forward kinematics on
//! the configuration that comes back, and measuring how far the tip actually
//! travelled. A cap that only bounds an intermediate quantity is not a cap.
//!
//! Every case runs on a 5-DOF chain and a 7-DOF one. An under-actuated chain
//! resolves a six-dimensional twist by least squares, so it is the one that
//! could overshoot a budget the algebra assumed was met exactly.

use chain_kinematics::nalgebra::{DMatrix, Isometry3, Translation3, UnitQuaternion, Vector3};
use chain_kinematics::{
    Chain, EeCaps, NoSmoothing, ServoLimits, ServoState, ServoStep, ServoTolerances,
    rate_step_toward,
};

mod common;
use common::{openarm, so101};

const DT: f64 = 0.01;

/// Generous per-joint budget, so the end-effector cap is the only thing binding.
fn limits<const N: usize>(linear_m_s: f64, angular_rad_s: f64) -> ServoLimits<N> {
    ServoLimits {
        max_joint_velocity: [8.0; N],
        ee: EeCaps {
            linear_m_s,
            angular_rad_s,
        },
        tolerances: ServoTolerances::new(1e-3, 1e-2).expect("a reachable tolerance"),
        dt_s: DT,
    }
}

fn tip<const N: usize>(chain: &Chain<N>, q: &[f64; N]) -> Isometry3<f64> {
    chain.world_pose(&chain.at(q).ee_pose())
}

/// Whether the Jacobian at `q` still spans the directions the step is resolved
/// in, measured by its smallest singular value.
///
/// Neither published helper answers this for both robots: `manipulability` is
/// `det(J Jt)`, identically zero on an under-actuated chain, and
/// `try_pseudo_inverse` refuses one outright. The singular value itself is
/// meaningful either way, so the test takes it directly.
fn well_conditioned<const N: usize>(chain: &Chain<N>, q: &[f64; N]) -> bool {
    let j = chain.at(q).jacobian();
    let svd = DMatrix::from_iterator(6, N, j.iter().copied()).svd(false, false);
    svd.singular_values
        .iter()
        .copied()
        .fold(f64::INFINITY, f64::min)
        > CONDITIONING_FLOOR
}

/// Peak tip speed (m/s) and slew (rad/s) over a run of steps toward `target`,
/// measured from forward kinematics on each configuration the law returns.
///
/// Reported twice: over every tick, and over only the ticks whose Jacobian still
/// spans the step. The two differ exactly at a singularity, which is the whole
/// point of measuring them apart.
struct Peaks {
    speed: f64,
    slew: f64,
    speed_well_conditioned: f64,
    conditioned_ticks: usize,
}

fn peak_toward<const N: usize>(
    chain: &Chain<N>,
    seed: [f64; N],
    target: &Isometry3<f64>,
    limits: &ServoLimits<N>,
    ticks: usize,
) -> Peaks {
    let mut q = seed;
    let mut p = Peaks {
        speed: 0.0,
        slew: 0.0,
        speed_well_conditioned: 0.0,
        conditioned_ticks: 0,
    };
    for _ in 0..ticks {
        let here = tip(chain, &q);
        let conditioned = well_conditioned(chain, &q);
        let next = rate_step_toward(chain, &q, &here, target, limits);
        let moved = tip(chain, &next);
        let speed = (moved.translation.vector - here.translation.vector).norm() / limits.dt_s;
        p.speed = p.speed.max(speed);
        p.slew = p
            .slew
            .max(here.rotation.angle_to(&moved.rotation) / limits.dt_s);
        if conditioned && well_conditioned(chain, &next) {
            p.speed_well_conditioned = p.speed_well_conditioned.max(speed);
            p.conditioned_ticks += 1;
        }
        q = next;
    }
    p
}

/// A pose far outside the chain's reach, in the direction it can least follow.
fn unreachable(from: &Isometry3<f64>) -> Isometry3<f64> {
    Isometry3::from_parts(
        Translation3::from(from.translation.vector + Vector3::new(50.0, -50.0, 50.0)),
        from.rotation,
    )
}

// The tip is what has a speed, so the tip is what gets measured. The margin
// covers one tick of curvature: the step is resolved through the Jacobian at the
// start of the tick, and the arc the tip then travels is not quite the chord the
// algebra sized.
const OVERSHOOT_MARGIN: f64 = 1.05;

/// Smallest singular value below which the Jacobian no longer spans the step.
const CONDITIONING_FLOOR: f64 = 1e-3;

#[test]
fn the_linear_cap_bounds_tip_speed_on_both_robots() {
    fn run<const N: usize>(chain: &Chain<N>, seed: [f64; N], label: &str) {
        for cap in [1.0, 0.5, 0.12, 0.03] {
            let l = limits::<N>(cap, 10.0);
            let target = unreachable(&tip(chain, &seed));
            let peaks = peak_toward(chain, seed, &target, &l, 300);
            assert!(
                peaks.conditioned_ticks > 20,
                "{label}: only {} well-conditioned ticks, so the bound was never exercised",
                peaks.conditioned_ticks
            );
            assert!(
                peaks.speed_well_conditioned <= cap * OVERSHOOT_MARGIN,
                "{label}: tip reached {:.4} m/s under a {cap} m/s cap while the Jacobian \
                 still spanned the step",
                peaks.speed_well_conditioned
            );
            // A cap that is never approached would pass the bound vacuously.
            assert!(
                peaks.speed_well_conditioned > cap * 0.5,
                "{label}: tip only reached {:.4} m/s under a {cap} m/s cap, \
                 so this case never pressed against the bound",
                peaks.speed_well_conditioned
            );
        }
    }
    run(&so101(), [0.3, -0.4, 0.5, -0.3, 0.2], "SO-101");
    run(&openarm(), [0.3, -0.4, 0.2, 0.8, -0.5, 0.3, 0.6], "OpenArm");
}

#[test]
fn quartering_the_cap_quarters_the_speed() {
    // Bounding the speed is not the same claim as governing it: a law that
    // always crawled would honour every cap and serve no one.
    fn run<const N: usize>(chain: &Chain<N>, seed: [f64; N], label: &str) {
        let target = unreachable(&tip(chain, &seed));
        let fast =
            peak_toward(chain, seed, &target, &limits::<N>(0.48, 10.0), 300).speed_well_conditioned;
        let slow =
            peak_toward(chain, seed, &target, &limits::<N>(0.12, 10.0), 300).speed_well_conditioned;
        let ratio = fast / slow;
        assert!(
            (3.0..=5.0).contains(&ratio),
            "{label}: quartering the cap changed peak speed by {ratio:.2}x \
             ({fast:.4} m/s vs {slow:.4} m/s), expected about 4x"
        );
    }
    run(&so101(), [0.3, -0.4, 0.5, -0.3, 0.2], "SO-101");
    run(&openarm(), [0.3, -0.4, 0.2, 0.8, -0.5, 0.3, 0.6], "OpenArm");
}

#[test]
fn distance_to_the_target_does_not_change_the_step() {
    // The documented promise: a target metres away produces the same bounded
    // step as one millimetres away. This is what lets one law serve both a
    // planned move and a live stream where the operator drags the target.
    fn run<const N: usize>(chain: &Chain<N>, seed: [f64; N], label: &str) {
        let here = tip(chain, &seed);
        let l = limits::<N>(0.25, 10.0);
        let mut seen = Vec::new();
        for reach in [0.05, 0.5, 5.0, 500.0] {
            let target = Isometry3::from_parts(
                Translation3::from(here.translation.vector + Vector3::new(reach, 0.0, 0.0)),
                here.rotation,
            );
            let next = rate_step_toward(chain, &seed, &here, &target, &l);
            let travelled = (tip(chain, &next).translation.vector - here.translation.vector).norm();
            assert!(
                travelled / DT <= 0.25 * OVERSHOOT_MARGIN,
                "{label}: a target {reach} m away produced {:.4} m/s",
                travelled / DT
            );
            seen.push(travelled);
        }
        // Everything past the first tick's reach is the same capped step.
        let far = &seen[1..];
        let spread = far.iter().cloned().fold(0.0f64, f64::max)
            - far.iter().cloned().fold(f64::INFINITY, f64::min);
        assert!(
            spread < 1e-9,
            "{label}: steps toward targets 0.5 m and 500 m away differ by {spread:.2e} m"
        );
    }
    run(&so101(), [0.3, -0.4, 0.5, -0.3, 0.2], "SO-101");
    run(&openarm(), [0.3, -0.4, 0.2, 0.8, -0.5, 0.3, 0.6], "OpenArm");
}

#[test]
fn the_angular_cap_bounds_tip_slew() {
    fn run<const N: usize>(chain: &Chain<N>, seed: [f64; N], label: &str) {
        let here = tip(chain, &seed);
        for cap in [2.0, 0.5, 0.1] {
            // Hold the position and ask for a large reorientation, so the slew
            // budget is the only thing the step is sized by.
            let target = Isometry3::from_parts(
                here.translation,
                UnitQuaternion::from_axis_angle(&Vector3::y_axis(), 2.5) * here.rotation,
            );
            let l = limits::<N>(10.0, cap);
            let slew = peak_toward(chain, seed, &target, &l, 120).slew;
            assert!(
                slew <= cap * OVERSHOOT_MARGIN,
                "{label}: tip slewed {slew:.4} rad/s under a {cap} rad/s cap"
            );
        }
    }
    run(&so101(), [0.3, -0.4, 0.5, -0.3, 0.2], "SO-101");
    run(&openarm(), [0.3, -0.4, 0.2, 0.8, -0.5, 0.3, 0.6], "OpenArm");
}

#[test]
fn the_tighter_of_the_two_budgets_is_the_one_that_binds() {
    // The joint budget and the end-effector budget are separate promises, and
    // both hold at once. Starve the joints and the tip must slow accordingly,
    // while no joint exceeds its own limit either way.
    let chain = openarm();
    let seed = [0.3, -0.4, 0.2, 0.8, -0.5, 0.3, 0.6];
    let target = unreachable(&tip(&chain, &seed));

    let mut starved = limits::<7>(1.0, 10.0);
    starved.max_joint_velocity = [0.05; 7];

    let mut q = seed;
    let mut fastest = 0.0f64;
    for _ in 0..200 {
        let here = tip(&chain, &q);
        let next = rate_step_toward(&chain, &q, &here, &target, &starved);
        for i in 0..7 {
            let rate = (next[i] - q[i]).abs() / DT;
            assert!(
                rate <= starved.max_joint_velocity[i] * 1.0001,
                "joint {i} moved at {rate:.4} rad/s against a {:.4} rad/s budget",
                starved.max_joint_velocity[i]
            );
        }
        fastest = fastest
            .max((tip(&chain, &next).translation.vector - here.translation.vector).norm() / DT);
        q = next;
    }
    assert!(
        fastest < 1.0,
        "the joint budget was the tighter one, yet the tip still reached {fastest:.4} m/s"
    );
}

#[test]
fn a_servo_move_respects_the_cap_end_to_end() {
    // The same budget through the whole state machine rather than one step:
    // reference walking, smoothing and re-clamping included.
    let chain = openarm();
    let seed = [0.3, -0.4, 0.2, 0.8, -0.5, 0.3, 0.6];
    let start = tip(&chain, &seed);
    let goal = tip(&chain, &[0.1, -0.2, 0.1, 1.4, -0.2, 0.1, 0.2]);
    let cap = 0.2;
    let l = limits::<7>(cap, 10.0);

    let mut state = ServoState::new(start, goal, 0.05, NoSmoothing);
    let mut q = seed;
    let mut fastest = 0.0f64;
    for _ in 0..3000 {
        let here = tip(&chain, &q);
        match state.step(&chain, &q, &l) {
            ServoStep::Stepped(next) => {
                fastest = fastest.max(
                    (tip(&chain, &next).translation.vector - here.translation.vector).norm() / DT,
                );
                q = next;
            }
            ServoStep::Converged(_) => break,
        }
    }
    assert!(
        fastest <= cap * OVERSHOOT_MARGIN,
        "a servo move peaked at {fastest:.4} m/s under a {cap} m/s cap"
    );
}

/// The budget is applied to a first-order model of the motion, so a tick that
/// crosses a singularity can outrun it. Pinned rather than fixed: the same
/// expression governs on `main`, and the OpenArm keeps its elbow off this
/// configuration with a lower limit of 0.05 rad, which the raw fixture URDF
/// (lower limit 0.0) deliberately does not carry.
///
/// The number matters. A step is sized from the Jacobian at the start of the
/// tick, and at a singularity that Jacobian under-predicts the arc the tip then
/// travels, so the cap holds approximately rather than exactly. This test says
/// how approximately, so that a change making it materially worse fails here,
/// and a change making it exact has somewhere to land.
#[test]
fn a_tick_through_a_singularity_can_outrun_the_cap() {
    let chain = openarm();
    let seed = [0.3, -0.4, 0.2, 0.8, -0.5, 0.3, 0.6];
    let cap = 1.0;
    let target = unreachable(&tip(&chain, &seed));
    let peaks = peak_toward(&chain, seed, &target, &limits::<7>(cap, 10.0), 300);

    assert!(
        peaks.speed > cap * OVERSHOOT_MARGIN,
        "the singular overshoot has gone away ({:.4} m/s under a {cap} m/s cap); if that \
         was deliberate, this test should be replaced by an exact-cap assertion",
        peaks.speed
    );
    assert!(
        peaks.speed <= cap * 1.30,
        "overshoot at the singularity grew to {:.4} m/s under a {cap} m/s cap, past the \
         1.30x this law has been measured at",
        peaks.speed
    );
    // It is only the singularity: away from it the same run holds the budget.
    assert!(
        peaks.speed_well_conditioned <= cap * OVERSHOOT_MARGIN,
        "the overshoot is not confined to the singular ticks"
    );
}
