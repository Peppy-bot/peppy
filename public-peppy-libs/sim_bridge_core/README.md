# sim_bridge_core

Rust shared library for Peppy sim node variants. Handles the translation between peppylib (raw JSON inside the sim process) and peppygen (typed Cap'n Proto messages in Peppy), with exponential backoff reconnect and cancellation on every pipeline.

Used by the Isaac Sim and MuJoCo variants of `openarm_backbone`, `openarm_arm`, `openarm_gripper`, and `uvc_camera`. Real variants talk directly to hardware via peppygen and do not depend on this library.

## What it provides

- **`SimBridge<Runner>`** — fluent builder that wires sim-to-OS and OS-to-sim pipelines and drives them concurrently
- **`run_sim_to_os` / `run_os_to_sim`** — pipeline runners with exponential backoff (`1 s → 30 s`) and `CancellationToken` support
- **`ArmMergeState`** — thread-safe shared state for merging independent per-arm joint commands into a single full-robot command vector
- **`BridgeConfig`** — JSON5 config loader with preset resolution via `PEPPY_BRIDGE_PRESET`
- **`DaemonState`** — core node name and messaging port, populated at startup via `peppylib::info`
- **`call_sim` / `call_sim_sync`** — peppylib-level sim control calls (reset, pause, step, set_joint_positions)
- **`BoxFuture<T>`** — `Pin<Box<dyn Future<Output = T> + Send + 'static>>` type alias for async callback signatures

## Adding as a dependency

```toml
[dependencies]
sim_bridge_core = { git = "https://github.com/Peppy-bot/public-peppy-libs", package = "sim_bridge_core" }
```

> `sim_bridge_core` depends on `peppylib` which is generated per-node by the peppy CLI. Run `peppy node sync` in your node directory before building.

## Configuration

Reads `config/sim_bridge.json5` by default. Set `PEPPY_BRIDGE_PRESET` to load a named preset from `config/presets/<preset>.json5` instead. Set `SIM_NODE` to override the sim node name at runtime.

## Usage

```rust
use std::sync::Arc;
use sim_bridge_core::{BoxFuture, DaemonState, SimBridge, read_bridge_config, sim_node_name};

let info = peppylib::info(&runner, None).await?;
let daemon = DaemonState {
    core_node_name: info.core_node_name,
    messaging_port: info.messaging_port,
};

let config = read_bridge_config()?;
let sim_node = sim_node_name(&config);
let token = runner.cancellation_token().clone();

SimBridge::new(runner.clone(), daemon, token, sim_node)
    .sim_to_os(Arc::from("joint_states"), |runner, msg: JointStatesMsg| -> BoxFuture<_> {
        Box::pin(async move {
            joint_states::emit(&runner, msg.robot, msg.step, msg.positions, msg.velocities, msg.stamp)
                .await
                .map_err(|e| e.to_string())
        })
    })
    .run()
    .await;
```
