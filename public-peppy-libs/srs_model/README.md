# srs_model

Kinematics and dynamics for a **7-DOF SRS arm** (spherical shoulder, revolute
elbow, spherical wrist): forward kinematics, closed-form arm-angle inverse
kinematics, and gravity/Coriolis validation on the real arm. Robot-agnostic - every dimension
comes from the URDF the caller supplies, and a chain that is not SRS is refused
at load. Pure Rust; no hardware, messaging, or async dependencies.

The topology-agnostic half of this lives in
[`chain_kinematics`](../chain_kinematics): the chain, the Jacobian, the damped
resolved-rate step, and the gravity/Coriolis feedforward. What stays here is
what is genuinely SRS; the dynamics are validated here, on this arm, against
KDL reference values (`tests/dynamics.rs`).

## Quick start

```rust
use srs_model::{Arm, ArmAnglePolicy};

let arm = Arm::from_urdf(urdf, "openarm_left_link0")?;

let posed = arm.at(&q);
let ee = posed.ee_pose();                 // arm base frame
let tau = posed.gravity_torques();        // world frame, payload included

let solution = arm.solve_ik(&target, ArmAnglePolicy::FromSeed, &q)?;
```

## What it does

| | |
|---|---|
| `Arm::at(q)` | pose for FK and dynamics; `&self`, so two configurations compare side by side |
| `Posed::{ee_pose, tip_pose}` | the tool control point, and the wrist before any tool |
| `Posed::{gravity_torques, coriolis_torques}` | feedforward dynamics (the chain's, at seven joints), payload included |
| `Posed::jacobian` | the 6x7 geometric Jacobian and its redundancy-aware inverses |
| `Arm::solve_ik` | closed-form arm-angle (Shimizu) IK under an [`ArmAnglePolicy`] |
| `Arm::arm_angle` | the arm angle a configuration is already at |
| `Arm::rate_step` | one damped resolved-rate step (`chain_kinematics`'s, at seven joints) |
| `Arm::chain` | the arm as a plain serial chain, for the generic laws |

## The redundancy

Seven joints reaching a six-dimensional pose leaves a one-parameter family of
solutions: the elbow sweeps a circle about the shoulder-wrist axis, and its
position on that circle is the **arm angle**. `solve_ik` resolves it by policy -
hold the seed's, take a fixed one, or maximise manipulability - and reports it
back on the [`Solution`], because on this robot the arm angle is not an
implementation detail: it is a planning tier, a jog mode, and a wire field.

The feasible arm angles are computed **analytically**, as the exact intervals of
psi that keep every joint in limits, rather than sampled. A sampled sweep misses
narrow feasible bands, and near a workspace edge that is where the answers are.
`tests/psi_completeness.rs` is the standing proof: 3000 seeded targets per side,
zero misses.

## Frames

FK and IK work in the **arm base frame** (the arm's own mounting link). Gravity
and Coriolis compute in the **world frame**, because gravity is a world quantity.
The fixed transform between them is captured at load from the URDF, so for
gravity to point the right way the URDF must carry the tree from its root down
to the mount, not just the bare arm chain.

Left and right are two chains in one URDF, selected by base link. The mirror is
re-derived from the geometry, never sign-flipped.

## What it refuses

`ArmModel::from_fk` checks that the chain really is SRS before anything trusts
the closed form: the three shoulder axes concurrent and orthonormal, the three
wrist axes likewise, the elbow axis perpendicular to and intersecting the
shoulder-wrist line, and the tip clear of the wrist centre. A chain that fails
any of them is an `Err`, not an approximation, because the closed form would
otherwise return a pose the arm cannot hold.

## Testing

```sh
cargo test          # unit, round-trip, psi completeness, parity fixture
cargo bench         # FK, Jacobian, dynamics and IK on the 100 Hz path
```

Four suites carry most of the weight:

- **`tests/psi_completeness.rs`** - the analytic feasible-psi intervals miss no
  reachable arm angle, over 3000 seeded targets per side.
- **`tests/round_trip.rs`** - IK inverts FK, per policy.
- **`tests/parity.rs`** - a recorded fixture of FK, Jacobians, dynamics, IK
  solutions and servo steps, asserted through the public API only. It is the
  parity gate: a refactor that changes a number fails here rather than on the
  robot. Regenerate deliberately with `SRS_PARITY_REGENERATE=1`, and never to
  make a change pass.
- **Dynamics** are checked against KDL `TreeIdSolver_RNE` reference values, so
  the branched gripper is included, and gravity additionally against the
  potential-energy gradient. FK is cross-checked against `k` (a dev-dependency
  only) as an independent implementation.
