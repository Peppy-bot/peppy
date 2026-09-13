# waldo_codegen fixture

A launch fixture that drives the waldo simulation engine's binding
generation through a real `stack_launch`: the node's full declarative
surface, the documents its contracts and pairings resolve from, and a
launcher that deploys one engine instance. Used by
`waldo_generation_launch_child` in `tests/listen_for_launch.rs`.

Every document here is declarative. No implementation code, asset, credential
or user configuration of the engine is part of the fixture.

## Sources

| File | Copied from | Revision |
| --- | --- | --- |
| `node/peppy.json5` (`manifest`, `interfaces`, `execution.parameters`) | `private-nodes-hub` `waldo/peppy.json5` | `70088e2e5e75e69e1ec8062425e3bb955126fa6b` |
| `contracts/scene_control.json5` | `contracts-hub` `simulation/scene_control.json5` | `0666e19509dfa4bbb423a5bd11426fd69f06ce3b` |
| `contracts/object_state.json5` | `contracts-hub` `simulation/object_state.json5` | `0666e19509dfa4bbb423a5bd11426fd69f06ce3b` |
| `contracts/contact_state.json5` | `contracts-hub` `simulation/contact_state.json5` | `0666e19509dfa4bbb423a5bd11426fd69f06ce3b` |
| `contracts/sensor_readout.json5` | `contracts-hub` `simulation/sensor_readout.json5` | `0666e19509dfa4bbb423a5bd11426fd69f06ce3b` |
| `pairings/joint_link.json5` | `pairings-hub` `robot/joint_link.json5` | `02b67dd852e2dfbcbd5cac9a6a2fc0812794748e` |
| `pairings/gripper_link.json5` | `pairings-hub` `robot/gripper_link.json5` | `02b67dd852e2dfbcbd5cac9a6a2fc0812794748e` |
| `pairings/sim_rgb_camera_link.json5` | `pairings-hub` `cameras/sim_rgb_camera_link.json5` | `02b67dd852e2dfbcbd5cac9a6a2fc0812794748e` |
| `pairings/sim_rgbd_camera_link.json5` | `pairings-hub` `cameras/sim_rgbd_camera_link.json5` | `02b67dd852e2dfbcbd5cac9a6a2fc0812794748e` |
| `peppy_launcher.json5` (`sim_inst` arguments) | `launchers-hub` `openarm/fragments/waldo_engine.json5`, with the scene commander adjustment applied | `3500c1e41b45d539f8774fb4c06b33996d2196c1` |

The contract and pairing documents are verbatim copies; their repositories
are Apache-2.0 licensed. The node manifest keeps the engine's declarative
structure and drops the engine's own commentary.

## What the fixture changes

- `execution` names no container and no engine binary. The node is an
  ordinary host Rust node whose `build_cmd` checks the generated bindings in
  the staged copy, prints `WALDO_CODEGEN_CHECKPOINT` only when every check
  passes, and then exits nonzero so the launch fails at the build and never
  starts an instance. `run_cmd` is never reached.
- The engine's crates deploy by symlink in the staged copy, as any host
  node's do; the container build the engine ships with deploys them by copy.
  The generated sources are the same either way.
- The launcher leaves every pairing slot vacant. The engine declares all of
  them optional, and generation does not depend on what a slot is bound to.
