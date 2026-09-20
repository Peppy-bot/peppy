# sim_robot_core

What a simulation engine node knows about the robots it stands, whatever its physics. The MuJoCo and Isaac Sim nodes of `nodes-hub` both install it in their base image and keep only what is their own: their gains, their model files, their physics and their transport.

| Module | What it holds |
|---|---|
| `registry` | The robots in the scene, by the name each stands under: who may join, who holds which robot, and the lease a robot keeps while its pairs are its model's. |
| `pairs` | The four pairing slots an engine declares (`arms`, `grippers`, `rgb_cameras`, `rgbd_cameras`) and what each pair names: its robot, and its limb or its camera. |
| `models` | One entry per robot model, with its checks, and the table that pairs those entries with an engine's own. |
| `cameras` | The camera configuration of a model's entry, the z16 depth wire format, and per-camera frame pacing. |

## The four slots

An engine declares one slot per kind of pair, each holding any number of pairs. Nothing in a slot's name says which robot, limb or camera a pair is, so every pair is read for it:

- a pair's robot is the copy it carries, which is the name the robot attached under. A pair with no copy belongs to a robot launched outside one, and outside a copy there is one robot in the scene for it to belong to;
- a limb pair's limb is the link the pair comes from on the robot's side, so a backbone names its downstream links after its limbs: `left_arm: "simulation_inst/arms"` for an OpenArm, `arm: "simulation_inst/arms"` for an SO-101;
- a camera pair's camera is its relay's name in the copy (`wrist_left`, `front`), which is the relay's instance id without the copy's prefix.

A new robot, limb or camera changes no engine's interface.

## Model entries

`src/sim_robot_core/models/<model>.json5` says what a robot of a model is made of: its limbs under the names the robot answers to in `limb_motion` and `limb_state`, the joints each limb moves, where each gripper's closed pose sits on its finger joints' range, the cameras the robot carries with the links they hang from, and the posture the robot starts in. The package ships `openarm_v1`, `openarm_v2` and `so101`. The SO-101 entry is held to [`so101_description`](../so101_description) by `tests/test_so101_entry.py`.

An engine keeps one entry of its own per model, `<model>.json5` in a directory beside its code, with what it alone knows: the file it loads, its gains, its extras. `Models.read(directory)` pairs the two, so an engine stands the models it has an entry for. A model with no entry is refused naming the ones the engine stands, and none falls back to another's layout.

```python
from sim_robot_core.models import Models

models = Models.read(engine_dir / "models")
known = models.of("so101")      # raises, naming the models this engine stands
known.entry.arm_names()          # ["arm"]
known.engine                     # the engine's own entry, as written
```

## The match

`ModelEntry.mismatch(held)` compares what a robot holds on the four slots with what its model has, and names both lists when they differ: a limb or a camera the model lacks, or a limb the model has and no pair drives. A camera the model has may go unpaired, since a camera nobody views is not rendered. An engine renews a robot's lease only while its pairs match, so a robot paired as another model does not stay, and `ModelEntry.holds_every_limb(held)` is what a robot's readiness asks.

## Tests

```sh
uv run pytest
```
