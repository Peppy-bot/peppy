# openarm_description

The single source of truth for the OpenArm V1.0 robot description. The URDF (and,
behind the `meshes` feature, the collision meshes) are embedded in the crate, so the
arm and backbone nodes share one description instead of each vendoring a copy.

Pure data: no kinematics or solver dependency. A consumer that wants a kinematic model
builds it from `urdf()` and applies the control margins itself, so the description
stays reusable by any consumer (a viz tool, a sim bridge) without pulling a solver in.

- `urdf()` returns the bundled URDF string.
- `write_meshes_to(dir)` (feature `meshes`) materializes the collision meshes for the
  file-based `bimanual_collision_model` builder.
- `ELBOW_SINGULARITY_FLOOR_RAD` / `ELBOW_JOINT_INDEX` describe the elbow control margin
  (see below).

It is a flat runtime URDF (xacro pre-expanded, no xacro at load) that includes the
`world -> openarm_body -> {left,right}_link0` mount tree, so gravity resolves in the
world frame. Structurally identical to enactic's `openarm_description` V1.0 example,
plus the parallel-gripper prismatic fingers so the distal-payload path is exercised
in production, not just in tests.

## Elbow singularity margin

The bundled URDF keeps the **mechanical** joint limits as vendored, including the
elbow (j4) lower bound of `0.0`. The control margin that holds the elbow off the
straight-arm singularity (where a closed-form arm-angle IK's redundancy reference is
undefined) is **not** baked into the URDF; it is exported as the constant
`ELBOW_SINGULARITY_FLOOR_RAD` (0.05 rad) for the kinematics consumer to apply to its
built model, e.g.

```rust
srs_model::Arm::from_urdf(openarm_description::urdf(), base_link)?
    .with_lower_floor(
        openarm_description::ELBOW_JOINT_INDEX,
        openarm_description::ELBOW_SINGULARITY_FLOOR_RAD,
    )
```

Keeping the margin out of the data separates the mechanical limit from the control
policy and keeps re-vendoring the upstream URDF clean.

## Gripper mass

The `hand_tcp` links carry the parallel-gripper **hand-body inertial** (0.127 kg,
COM at z ≈ 0.102 m from link7), taken from enactic `openarm_description`
`assets/end_effector/parallel_link/config/inertials.yaml`, alongside the two 36 g
fingers. This is the one physical correction to the vendored URDF (upstream omits the
hand-body inertials). `srs_model` lumps everything past the wrist into the distal
payload (`Payload::from_distal` walks every descendant of the wrist), so the full
~0.2 kg gripper is in the gravity/Coriolis feedforward.

This matches the **parallel-link (2-finger) gripper this robot uses**, not the heavier
single-body `openarm_hand` (0.35 kg) the openarm_teleop launch scripts target, which
is a different end effector. The arm holds the wrist with a position setpoint plus
this feedforward, so the gripper-body mass is compensated and the wrist holds without
sag.
