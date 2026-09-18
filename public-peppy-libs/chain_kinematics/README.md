# chain_kinematics

Forward kinematics, the geometric Jacobian, and damped resolved-rate inverse
kinematics for **any serial chain** read from a URDF. Generic over the number of
joints, with no assumption about topology: a seven-axis arm, a five-axis arm, a
leg, a pan-tilt head. Pure Rust; no hardware, messaging, or async dependencies.

This is the Rust sibling of [`chain_kinematics_py`](../chain_kinematics_py),
which solves the same problem on placo's QP for Python nodes.

## Quick start

```rust
use chain_kinematics::{Chain, ChainSpec, JointSelection};

let robot = urdf_rs::read_from_string(urdf)?;
let chain = Chain::<5>::from_urdf(&robot, &ChainSpec {
    base_link: None,                       // None = the URDF root
    tip_link: "gripper_frame_link",        // named, not discovered
    joints: JointSelection::Named(&[
        "shoulder_pan", "shoulder_lift", "elbow_flex", "wrist_flex", "wrist_roll",
    ]),
})?;

let ee = chain.at(&[0.0; 5]).ee_pose();
let j = chain.at(&[0.0; 5]).jacobian();
```

## What the caller supplies

A parsed URDF and a `ChainSpec`. Two of its three fields are deliberately
explicit rather than inferred:

- **`tip_link` is named.** Discovering a tip by walking out and counting joints
  only ever works for the arm the rule was written for.
- **`joints` may be named, and then the order is the order of `q`.** Inferring
  joint order from the URDF's own ordering is luck rather than a contract: `q`'s
  order is usually the robot's wire order, which no URDF knows about. A movable
  joint on the path that is not named is frozen at zero and folded in as a fixed
  offset, so a gripper branching off the last link does not have to be actuated.

`N` is checked against the URDF once, at load. After that every type is
fixed-size and a control tick allocates nothing.

## What it refuses at load, and why

Each of these would otherwise produce a plausible wrong answer rather than an
error, so it is a refusal at construction rather than a caveat downstream. The
list matches `chain_kinematics_py`'s, which arrived at the same rules against a
different solver.

| Refused | What you would get otherwise |
|---|---|
| A joint named twice in `JointSelection::Named` | The later slot wins, so the earlier entry of `q` moves nothing: the caller commands two joints and the arm honours one |
| An actuated joint with no usable range (absent `<limit>`, or an infinite or inverted one) | A joint that turns freely confined to an invented range, so the chain refuses motion the mechanism has |
| A `<mimic>` coupling with either end among the actuated joints | `q` poses only the joints it names, so the coupled joint stays behind and the reported tip is a pose the mechanism cannot hold. A coupling clear of the chain, as a gripper's two fingers are, is left alone |
| A tip that is not below the base, or a joint that is not on the path | A frame composed from an unrelated branch |
| A tool link reached through a joint that moves | A "fixed" tool offset that tracks the gripper opening |
| A URDF that is not a single tree, or `N` disagreeing with the chain's joint count | Every fixed-size type below built on a count that is not the robot's |

## What it does

| | |
|---|---|
| `Chain::at(q)` | pose the chain; pure, so `Chain` is `Send + Sync` |
| `Posed::{ee_pose, tip_pose, link_pose_world}` | frames, in the base frame the spec named |
| `Posed::jacobian` | the 6xN geometric Jacobian, revolute and prismatic columns |
| `Posed::point_world_jacobian` | the same, for an arbitrary witness point - what a collision-distance gradient needs |
| `Posed::{mass, com_world, inertia_world}` | per-segment rigid-body data |
| `Posed::{gravity_torques, coriolis_torques}` | feedforward dynamics, revolute and prismatic joints alike, with everything each segment carries folded in |
| `damped_pseudo_inverse` | `Jᵀ(J Jᵀ + λ²I)⁻¹`, defined everywhere including at singularities |
| `try_pseudo_inverse` | Moore-Penrose, `None` at a singularity and on a chain of fewer than six joints, whose pseudo-inverse exists but is not a right inverse, so no joint rates realize an arbitrary twist; use the damped one for both |
| `null_space_projector` | `I − J⁺J`, for a secondary objective that must not disturb the end effector |
| `manipulability` | Yoshikawa index, the product of the singular values, at any joint count |
| `rate_step` | one damped resolved-rate step under a velocity budget and joint limits |
| `ServoState` / `rollout` | a guarded Cartesian move, and the same law rolled out offline as its reachability proof |

## Point-to-point: `inverse_kinematics`, `continue_to`, `track`

The `Kinematics` type carries the surface frozen across this library and
`chain_kinematics_py`: three entry points that answer different questions and
whose refusals mean different things.

| Entry | For | Search scope | A refusal means |
|---|---|---|---|
| `inverse_kinematics` | a planned move | whole workspace | nothing found reached it |
| `continue_to` | following a path | the seed's neighbourhood only | this neighbourhood does not reach it |
| `track` | streamed setpoints | one bounded descent | never refuses |

An accepted solution is proven: forward kinematics on the chain itself puts it
inside the tolerance. A refusal is the search giving up within its budget, not
a proof of unreachability; a general chain has no closed form to prove with.
Position alone is the acceptance test unless the goal names an orientation
tolerance, because a chain under six actuated joints cannot meet an arbitrary
orientation. `track` always returns an in-limit configuration and comes to rest
against an unreachable target, so a held target gives a held answer rather than
walking the arm around the workspace boundary.

Targets are world-frame `Isometry3` values; the typed rotation is what keeps a
wire quaternion's component order from being confused past the boundary.

Qualification, `cargo test --release --test ik_quality` (poses reachable by
construction, seeds unrelated to the answers):

| | refused | worst position | orientations abandoned (>1 deg) |
|---|---|---|---|
| SO-101, 5 DOF, 1000 poses | 0 | 9.0e-4 m | 1 (worst 2.1 deg) |
| OpenArm, 7 DOF, 300 poses | 0 | 8.5e-4 m | 0 |

## The servo law

A discrete IK walk cannot cross the singular surface between two solution
branches. The damped law can: the damping bounds the joint rates while the task
error carries the chain across, deviating from the straight line only where the
geometry forces it. The reference is *leashed* - it stops advancing while the
chain is behind it - so a wall is ground through instead of the goal running
away.

`rollout` runs the identical law offline before a move is accepted, which is the
reachability check: a goal that does not converge within the caller's budget is
refused rather than started. That only works because the law is deterministic,
so the motion that was validated is the motion that runs.

The step always returns a configuration. An unreachable target moves toward the
closest pose the step can find and saturates there rather than refusing, because
a teleoperator pushing past the edge should feel a wall, not a disconnect.

Tolerances and the output smoother are the caller's: `Smoother` is a trait, so
this crate needs no filter library and the servo's output is smoothed exactly as
the rest of that controller's commands are. Arrival slack is parsed rather than
taken as given (`ServoTolerances::new`): a step stops correcting position inside
`TRACKING_FLOOR_M`, so a tolerance tighter than that is one the law would never
meet, and it is refused where the number is given instead of timing the move out.

## Layout

```text
src/
  lib.rs        Chain, ChainSpec, JointSelection, Posed, Limit
  tree.rs       the URDF arena: links, joints, adjacency, and the walks over it
  jacobian.rs   the Jacobian's inverses, generic over N
  rate.rs       one damped resolved-rate step
  ik.rs         point-to-point IK: searched, verified, willing to refuse
  dynamics.rs   gravity and Coriolis feedforward over the posed chain
  servo.rs      the leashed-reference Cartesian move and its offline rollout
  error.rs      what can go wrong building a chain
  payload.rs    what each segment carries beyond its own link, lumped in
```

## Testing

`cargo test` drives the URDF models of two real robots through the same API - a
five-joint SO-101 and a seven-joint OpenArm - checking that every Jacobian column
matches a central difference of the pose, that the servo law converges on both,
that naming the joints fixes the order of `q`, that a wrong joint count is
refused at load rather than silently ignoring part of `q`, and that the damped
inverse stays finite on an under-actuated (6x5) Jacobian. No hardware is
involved; qualifying the law on a physical arm is a separate exercise.

`tests/refusals.rs` covers the table above: every rule is checked both ways, so a
refusal that would also reject a valid chain fails the suite.
`tests/ik_quality.rs` is the point-to-point qualification: the debug build
runs a fast subset of the same draws, and `--release` runs the full scale the
numbers above are quoted at.
