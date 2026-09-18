# sim_ext_core

Python shared package for Peppy Isaac Sim and MuJoCo extensions. Provides the plugin lifecycle base, peppylib transport with reconnect backoff, config loading, and bridge plugins for all sensor types.

Used by the Isaac and MuJoCo extension variants of `openarm_backbone`, `openarm_arm`, `openarm_gripper`, and `uvc_camera`. Real variants do not use this package.

## What it provides

- **`BridgePlugin`** — ABC with shared `try_setup()` concrete behaviour. Implement `setup()`, `on_step()`, `teardown()`, and `is_ready`
- **`PeppylibIO`** — peppylib transport running in a background asyncio thread with exponential backoff reconnect
- **`peppylib_session`** — context manager that manages `PeppylibIO` start/stop lifecycle
- **`BridgeConfig`** — frozen dataclass for extension config, loaded from JSON5 file or env vars
- **Bridge plugins** — ready-made plugins for all sensor types (see below)

## Adding as a dependency

```toml
[project]
dependencies = ["sim_ext_core"]

[tool.uv.sources]
sim_ext_core = { git = "https://github.com/Peppy-bot/public-peppy-libs", subdirectory = "sim_ext_core" }
```

## Available bridge plugins

| Plugin | Direction | Topic |
|---|---|---|
| `JointStatesBridge` | READ | joint_states |
| `ImuBridge` | READ | imu |
| `EePoseBridge` | READ | ee_pose |
| `TfTreeBridge` | READ | tf_tree |
| `OdometryBridge` | READ | odometry |
| `WrenchBridge` | READ | wrench |
| `ContactForcesBridge` | READ | contact_forces |
| `GripperStateBridge` | READ | gripper_state |
| `SimControlBridge` | READ/WRITE | sim control |

## Usage

Each extension's `__init__.py` injects `sim_ext_core` into `sys.path`. The extension class drives a list of `BridgePlugin` instances, calling `try_setup()` on each physics step until ready, then `on_step()` each frame:

```python
from sim_ext_core import BridgePlugin, BridgeConfig, peppylib_session
from sim_ext_core.bridges import JointStatesBridge, ImuBridge

class PeppyBackboneExtension(omni.ext.IExt):
    def on_startup(self, ext_id: str) -> None:
        self._config = BridgeConfig.from_file()
        self._plugins: list[BridgePlugin] = [
            JointStatesBridge(articulation, self._config, publisher_entry),
            ImuBridge(imu_sensor, self._config, publisher_entry),
        ]

    def _on_physics_step(self, step: int) -> None:
        with peppylib_session(self._config) as io:
            for plugin in self._plugins:
                if not plugin.is_ready:
                    plugin.try_setup()
                else:
                    plugin.on_step(step, io)
```
