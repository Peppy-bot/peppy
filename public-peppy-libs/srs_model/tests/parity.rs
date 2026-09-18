//! Behaviour parity fixture.
//!
//! Records every number the OpenArm control path reads out of this crate, for a
//! deterministic sweep of configurations, and asserts them against a committed
//! file. It exists so that a refactor of the internals - replacing the URDF/FK
//! backend, moving the differential-kinematics layer into its own crate - can be
//! shown to change nothing the robot computes, rather than argued to.
//!
//! Everything goes through the **public** API (`Arm`, `Posed`), so the fixture
//! stays valid across internal reorganisation and only fails when observable
//! behaviour moves.
//!
//! Regenerate deliberately, never to make a red test green:
//! `SRS_PARITY_REGENERATE=1 cargo test --release --test parity`
//!
//! `TOLERANCE` is 0 by default: extraction and code motion must be bit-exact. A
//! change that genuinely reassociates floating-point work (a linear-algebra
//! version bump) is run once with `SRS_PARITY_TOLERANCE=1e-9` to show the
//! difference is last-ulp, and the reason is recorded in the commit.

mod common;

use std::fmt::Write as _;

use srs_model::nalgebra::{Isometry3, Vector3};
use srs_model::{ARM_DOF, ArmAnglePolicy, JointVec, Limit};

/// Deterministic in-limit sweep. A fixed lattice rather than an RNG so the
/// fixture does not depend on any `rand` version.
fn sweep(limits: &[Limit; ARM_DOF], count: usize) -> Vec<JointVec> {
    // Golden-ratio (additive recurrence) sampling: low-discrepancy, so a modest
    // count still covers the joint box evenly, and reproducible anywhere.
    const PHI: f64 = 0.618_033_988_749_894_9;
    (0..count)
        .map(|k| {
            std::array::from_fn(|i| {
                let t = ((k + 1) as f64 * PHI * (i + 1) as f64).fract();
                limits[i].lo + t * (limits[i].hi - limits[i].lo)
            })
        })
        .collect()
}

/// Collected `label -> values` records, written or compared as a whole.
#[derive(Default)]
struct Parity(Vec<(String, Vec<f64>)>);

impl Parity {
    fn push(&mut self, label: impl Into<String>, values: impl IntoIterator<Item = f64>) {
        self.0.push((label.into(), values.into_iter().collect()));
    }

    fn push_pose(&mut self, label: impl Into<String>, p: &Isometry3<f64>) {
        let t = p.translation.vector;
        let r = p.rotation.coords; // (i, j, k, w)
        self.push(label, [t.x, t.y, t.z, r.x, r.y, r.z, r.w]);
    }

    fn render(&self) -> String {
        let mut out = String::new();
        for (label, values) in &self.0 {
            let _ = write!(out, "{label}");
            for v in values {
                // 17 significant digits round-trips an f64 exactly.
                let _ = write!(out, " {v:.17e}");
            }
            out.push('\n');
        }
        out
    }

    /// Compare against `recorded`, reporting the first disagreement with enough
    /// context to identify which quantity moved.
    fn assert_matches(&self, recorded: &str, tolerance: f64) {
        let mut lines = recorded.lines().filter(|l| !l.trim().is_empty());
        for (label, values) in &self.0 {
            let line = lines
                .next()
                .unwrap_or_else(|| panic!("parity file ended early, expected '{label}'"));
            let mut fields = line.split_whitespace();
            let got_label = fields.next().expect("parity line has a label");
            assert_eq!(got_label, label, "parity records out of order");
            let want: Vec<f64> = fields
                .map(|f| f.parse().expect("parity value parses as f64"))
                .collect();
            assert_eq!(
                want.len(),
                values.len(),
                "{label}: parity has {} values, produced {}",
                want.len(),
                values.len()
            );
            for (i, (&got, &exp)) in values.iter().zip(&want).enumerate() {
                // A recorded NaN is a real datum - an IK miss, or an undefined
                // arm angle - so it has to compare equal to itself in both modes,
                // which a difference test never does.
                let ok = if got.is_nan() || exp.is_nan() {
                    got.is_nan() && exp.is_nan()
                } else if tolerance == 0.0 {
                    got.to_bits() == exp.to_bits()
                } else {
                    (got - exp).abs() <= tolerance * exp.abs().max(1.0)
                };
                assert!(
                    ok,
                    "{label}[{i}]: produced {got:.17e}, parity {exp:.17e} \
                     (delta {:.3e}, tolerance {tolerance:.0e})",
                    got - exp
                );
            }
        }
        assert!(
            lines.next().is_none(),
            "parity file has more records than were produced"
        );
    }
}

/// Every quantity the OpenArm nodes read, for one side.
fn record(side: &str) -> Parity {
    let arm = common::arm(side);
    let limits = arm.limits();
    let mut g = Parity::default();

    g.push_pose("base_from_world", &arm.base_from_world());
    g.push_pose("tool", &arm.tool());
    g.push(
        "limits",
        limits.iter().flat_map(|l| [l.lo, l.hi]).collect::<Vec<_>>(),
    );

    let configs = sweep(&limits, 64);
    for (k, q) in configs.iter().enumerate() {
        // Forward kinematics and the frames the collision model composes against.
        let posed = arm.at(q);
        let ee = posed.ee_pose();
        let tip = posed.tip_pose();
        let jac = posed.jacobian();
        let grav = posed.gravity_torques();
        let qdot: JointVec = std::array::from_fn(|i| 0.1 * (i as f64 + 1.0) * (k as f64).cos());
        let cor = posed.coriolis_torques(&qdot);
        let links: Vec<f64> = (0..ARM_DOF)
            .flat_map(|i| {
                let p = posed.link_pose_world(i);
                let t = p.translation.vector;
                let r = p.rotation.coords;
                [t.x, t.y, t.z, r.x, r.y, r.z, r.w]
            })
            .collect();
        // The witness-point Jacobian the collision governor differentiates.
        let witness = srs_model::nalgebra::Point3::new(0.1, -0.2, 0.5);
        let pwj: Vec<f64> = (0..ARM_DOF)
            .flat_map(|seg| {
                posed
                    .point_world_jacobian(&witness, seg)
                    .into_iter()
                    .flat_map(|v| [v.x, v.y, v.z])
                    .collect::<Vec<_>>()
            })
            .collect();

        g.push(format!("q[{k}]"), *q);
        g.push_pose(format!("ee[{k}]"), &ee);
        g.push_pose(format!("tip[{k}]"), &tip);
        g.push(format!("jac[{k}]"), jac.iter().copied().collect::<Vec<_>>());
        g.push(format!("grav[{k}]"), grav);
        g.push(format!("cor[{k}]"), cor);
        g.push(format!("links[{k}]"), links);
        g.push(format!("pwj[{k}]"), pwj);

        // World-frame round trip, and the arm angle the panel displays.
        g.push_pose(format!("world_ee[{k}]"), &arm.world_pose(&ee));
        g.push(
            format!("arm_angle[{k}]"),
            [arm.arm_angle(q).unwrap_or(f64::NAN)],
        );
    }

    // Inverse kinematics across all three redundancy policies, seeded from a
    // different configuration so the feasible-interval search runs.
    for (k, q) in configs.iter().enumerate().take(32) {
        let target = arm.at(q).ee_pose();
        let seed = configs[(k + 7) % configs.len()];
        for (name, policy) in [
            ("from_seed", ArmAnglePolicy::FromSeed),
            (
                "max_manip",
                ArmAnglePolicy::MaxManipulability { max_step_rad: 0.25 },
            ),
            (
                "fixed",
                ArmAnglePolicy::Fixed(arm.arm_angle(q).unwrap_or(0.0)),
            ),
        ] {
            let label = format!("ik_{name}[{k}]");
            match arm.solve_ik(&target, policy, &seed) {
                // A miss is recorded as such: a refactor that starts or stops
                // solving a target is exactly what this fixture must catch.
                None => g.push(label, [f64::NAN]),
                Some(s) => g.push(label, s.q.into_iter().chain([s.arm_angle])),
            }
        }
    }

    // Targets pressed up against the straight-arm reach boundary. The solver's
    // reach gate is a 1e-9 band, so without a target inside it the gate's width
    // is unconstrained and could be widened by orders of magnitude unnoticed.
    // `d(theta4) ~ (l_su + l_uw) - 0.0545 * theta4^2` on this arm, so these
    // elbow flexions land microns to nanometres inside maximum reach.
    for (k, elbow) in [5e-4_f64, 1e-3, 5e-3, 5e-2].into_iter().enumerate() {
        let q: JointVec = [0.2, -0.3, 0.25, elbow, -0.35, 0.4, 0.15];
        let target = arm.at(&q).ee_pose();
        g.push_pose(format!("reach_edge_target[{k}]"), &target);
        let seed: JointVec = [0.1, -0.2, 0.15, elbow.max(1e-4), -0.3, 0.25, 0.2];
        match arm.solve_ik(&target, ArmAnglePolicy::FromSeed, &seed) {
            None => g.push(format!("reach_edge[{k}]"), [f64::NAN]),
            Some(s) => g.push(
                format!("reach_edge[{k}]"),
                s.q.into_iter().chain([s.arm_angle]),
            ),
        }
    }

    // A degenerate damping value, which the DLS inverse is documented to clamp
    // to an internal floor rather than inverting a singular matrix. Without this
    // the floor's magnitude is unconstrained.
    for (k, lambda) in [0.0_f64, -0.05, f64::NAN].into_iter().enumerate() {
        let q: JointVec = [0.3, 0.1, 0.2, 0.8, 0.3, 0.2, 0.15];
        let out = arm.rate_step(
            &q,
            Vector3::new(2e-3, -1e-3, 5e-4),
            Vector3::new(1e-3, 2e-3, -1e-3),
            &[2.0; ARM_DOF],
            0.01,
            lambda,
        );
        g.push(format!("degenerate_lambda[{k}]"), out);
    }

    // Chained damped resolved-rate steps: the servo law's inner loop, run long
    // enough that any drift in the DLS inverse compounds visibly.
    let v_max: JointVec = [2.0; ARM_DOF];
    for (k, start) in configs.iter().enumerate().take(8) {
        let mut q = *start;
        for step in 0..24 {
            let phase = step as f64 * 0.3;
            let dp = Vector3::new(1e-3 * phase.cos(), 1e-3 * phase.sin(), 5e-4);
            let dw = Vector3::new(2e-3 * phase.sin(), 1e-3, 2e-3 * phase.cos());
            q = arm.rate_step(&q, dp, dw, &v_max, 0.01, srs_model::DEFAULT_DLS_LAMBDA);
        }
        g.push(format!("rate_chain[{k}]"), q);
    }

    g
}

#[test]
fn matches_the_recorded_behaviour() {
    let tolerance: f64 = std::env::var("SRS_PARITY_TOLERANCE")
        .ok()
        .map(|v| v.parse().expect("SRS_PARITY_TOLERANCE parses as f64"))
        .unwrap_or(0.0);
    let regenerate = std::env::var_os("SRS_PARITY_REGENERATE").is_some();

    for side in ["left", "right"] {
        let path = format!(
            "{}/tests/fixtures/parity_v10_{side}.txt",
            env!("CARGO_MANIFEST_DIR")
        );
        let produced = record(side);
        if regenerate {
            std::fs::write(&path, produced.render()).expect("write parity fixture");
            println!("regenerated {path}");
            continue;
        }
        let recorded = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("read {path}: {e} (regenerate with SRS_PARITY_REGENERATE=1)")
        });
        produced.assert_matches(&recorded, tolerance);
    }
    assert!(
        !regenerate,
        "parity fixtures regenerated; re-run without SRS_PARITY_REGENERATE to check them in"
    );
}
