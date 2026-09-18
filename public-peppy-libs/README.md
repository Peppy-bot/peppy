# public-peppy-libs

Shared infrastructure libraries for Peppy nodes. Most libraries in this repo are independent packages pulled as a git dependency by nodes that require it; `peppy-shared` is a Cargo workspace of public-facing crates (including the `peppylib` control library) consumed inside the peppy superproject.

Peppy nodes live in separate repositories under the nodes hub. Shared code that is needed across multiple independent node repos cannot be a path dependency inside one node's repo — it needs a central place. This repo is that central place.

## Libraries

| Library | Language | Purpose |
|---|---|---|
| [openarm_can](./openarm_can) | Rust | Safe wrapper around the `openarm_can` C++ library for driving the physical OpenArm hardware over CAN (`ArmCan` / `GripperCan`, Damiao motor types, OpenArm v10 constants) |
| [chain_kinematics](./chain_kinematics) | Rust | Forward kinematics, the geometric Jacobian, and damped resolved-rate inverse kinematics for any serial chain read from a URDF - generic over the number of joints, no topology assumed. The Rust sibling of `chain_kinematics_py` |
| [srs_model](./srs_model) | Rust | Kinematics and dynamics for a 7-DOF SRS arm: FK, closed-form arm-angle (Shimizu) IK, gravity/Coriolis feedforward, and Jacobians. Robot-agnostic: all geometry derives from the supplied URDF. Builds on `chain_kinematics`; pure Rust, no hardware or messaging deps |
| [bimanual_collision_model](./bimanual_collision_model) | Rust | Runtime self-collision detection for a bimanual robot — URDF-fitted convex hulls, GJK/EPA minimum distance, the analytic distance gradient, and a proximity band for scaling commanded motion near contact. Builds on `srs_model`; pure Rust, no hardware or messaging deps |
| [openarm_description](./openarm_description) | Rust | The OpenArm v1.0 robot description as a single embedded source of truth — the URDF, the collision meshes (behind the `meshes` feature), and the elbow singularity control-margin constants. Pure data: no kinematics or solver deps, so any consumer (`srs_model`, a viz tool, a sim bridge) builds from `urdf()` itself |
| [sim_bridge_core](./sim_bridge_core) | Rust | raw-to-peppygen pipelines for Isaac Sim and MuJoCo bridge nodes; the node supplies the peppylib transport |
| [sim_ext_core](./sim_ext_core) | Python | Plugin lifecycle, config loading, and sensor bridge plugins for the in-sim extensions; the node supplies the IO transport |
| [control_core](./control_core) | Rust | Shared control-loop primitives for the openarm control nodes: a fixed-rate `Pacer` with overrun accounting |
| [peppy-shared](./peppy-shared) | Rust + Python | Cargo workspace of public-facing Peppy crates migrated out of the `peppy` workspace — the `peppylib` control library and its Python bindings, plus the messaging, config, and core-node API crates they build on (see [below](#peppy-shared-crates)) |

### peppy-shared crates

A virtual Cargo workspace. Crates are migrated here from the private `peppy` workspace PR by PR, so they sit at the bottom of the dependency graph and are shared by both workspaces.

| Crate | Language | Purpose |
|---|---|---|
| [peppylib-rs](./peppy-shared/peppylib-rs) (`peppylib`) | Rust | The Peppy control library — messaging, core-node helpers, runtime, services, config, and types |
| [peppylib-py](./peppy-shared/peppylib-py) | Python | PyO3 bindings exposing the `peppylib` control library to Python; published to PyPI as `peppylib` |
| [peppy-messaging-interface](./peppy-shared/peppy-messaging-interface) (`pmi`) | Rust | Messaging transport interface — zenoh transport plus an in-process mock adapter, sessions, and org-id namespace routing |
| [peppy-config-model](./peppy-shared/peppy-config-model) | Rust | Parsing and validation of the shared Peppy config documents: the `peppy.json5` node config model, runtime configs shipped to nodes, codegen fingerprints, and schema tags |
| [core-node-api](./peppy-shared/core-node-api) | Rust | Shared API surface for talking to a core-node daemon — capnp request/response types, service-name constants, and response parsers |
| [peppy-mcp-catalog](./peppy-shared/peppy-mcp-catalog) | Rust | The `mcp_exposure/v1` document model, its validation against the contracts it names (the canonical `message_format` to JSON Schema mapping included), and the versioned exposure catalog that validation derives, shared by the `peppy` binary and the MCP server runtime |
| [peppy-mcp-runtime](./peppy-shared/peppy-mcp-runtime) | Rust | The MCP server runtime the `peppy` binary serves exposures with: one Streamable HTTP listener on `127.0.0.1` speaking MCP `2026-07-28`, built on the official `rmcp` SDK, with one endpoint per exposure at `/<name>/<tag>/mcp` serving that exposure's resources, tools, and action-backed tasks with freshness, rate, representation, size, confirmation, and deadline policies |
| [json5-pretty](./peppy-shared/json5-pretty) | Rust | Pretty-print a `Serialize` value as JSON5 with unquoted object keys |
| [config-test-support](./peppy-shared/config-test-support) | Rust | Test fixtures shared across the Peppy workspaces (scratch dirs, and git-repo / node-config-template fixtures behind a feature) |
| [build-helpers](./peppy-shared/build-helpers) | Rust | Generic build-script helpers shared across peppy crates |

## Using these libraries

Rust nodes pin `sim_bridge_core` in `Cargo.toml`:

```toml
[dependencies]
sim_bridge_core = { git = "https://github.com/Peppy-bot/public-peppy-libs", rev = "<commit>" }
```

Python consumers install `sim_ext_core` from a pinned commit:

```
sim_ext_core @ git+https://github.com/Peppy-bot/public-peppy-libs.git@<commit>#subdirectory=sim_ext_core
```

Pin a commit rather than a branch: node builds happen inside containers and should be reproducible. When this repo changes, bump the pin in the consuming node and rebuild it.

The `peppy-shared` crates are different: they are consumed inside the peppy superproject as sibling-submodule path dependencies (`peppy` and `public-peppy-libs` checked out side by side) rather than via git-rev pins, and Python consumers install `peppylib` from PyPI.

See each library's README for the API.
