# public-peppy-libs

Shared infrastructure libraries for Peppy nodes. Most libraries in this tree are independent packages pulled as a git dependency by nodes that require it; `peppy-shared` is a Cargo workspace of public-facing crates (including the `peppylib` control library) shared by the `peppy` workspace and `platform-backend`.

Peppy nodes live in separate repositories under the nodes hub. Shared code that is needed across multiple independent node repos cannot be a path dependency inside one node's repo: it needs a central place. This tree is that central place.

## A sealed tree

This directory lives inside the [`peppy`](https://github.com/Peppy-bot/peppy) repository but is not part of the `peppy` workspace, whose root manifest excludes it. `peppy-shared` is a workspace of its own with its own `Cargo.lock`, and every other library here is a standalone package: its manifest carries an empty `[workspace]` table, which makes the package its own workspace root, so it builds the same whatever directory the tree is checked out under.

The dependency between the two runs one way only:

- The `peppy` crates (`crates/`) depend on the crates here, by path.
- Nothing here depends on `crates/`, or on anything else outside this directory. The libraries depend on each other and on the registry.

That is what lets `platform-backend` and the hub nodes consume these libraries from the repository without building a line of `peppy` itself. Two checks hold the line:

- [`peppy-shared/build-helpers/tests/sealed_tree.rs`](./peppy-shared/build-helpers/tests/sealed_tree.rs) reads every manifest in the tree and fails, naming the manifest and key, when a `path` (a dependency of any kind, a `[patch]`, a target's source file, a workspace root or member) or a symlink resolves outside it, and when a package would send cargo looking for its workspace root above the tree.
- CI runs this tree's suites from a copy that holds nothing but the tree, so a reach the manifests cannot show (`#[path]`, `include!`) has nothing to resolve to.

If a library here needs something that lives in `crates/`, move that code into the tree.

## License

Everything in this directory is licensed under the [Apache License, Version 2.0](./LICENSE), separately from the rest of the `peppy` repository.

## Libraries

| Library | Language | Purpose |
|---|---|---|
| [openarm_can](./openarm_can) | Rust | Safe wrapper around the `openarm_can` C++ library for driving the physical OpenArm hardware over CAN (`ArmCan` / `GripperCan`, Damiao motor types, OpenArm v10 constants) |
| [chain_kinematics](./chain_kinematics) | Rust | Forward kinematics, the geometric Jacobian, and damped resolved-rate inverse kinematics for any serial chain read from a URDF - generic over the number of joints, no topology assumed. The Rust sibling of `chain_kinematics_py` |
| [srs_model](./srs_model) | Rust | Kinematics and dynamics for a 7-DOF SRS arm: FK, closed-form arm-angle (Shimizu) IK, gravity/Coriolis feedforward, and Jacobians. Robot-agnostic: all geometry derives from the supplied URDF. Builds on `chain_kinematics`; pure Rust, no hardware or messaging deps |
| [bimanual_collision_model](./bimanual_collision_model) | Rust | Runtime self-collision detection for a bimanual robot: URDF-fitted convex hulls, GJK/EPA minimum distance, the analytic distance gradient, and a proximity band for scaling commanded motion near contact. Builds on `srs_model`; pure Rust, no hardware or messaging deps |
| [openarm_description](./openarm_description) | Rust | The OpenArm v1.0 robot description as a single embedded source of truth: the URDF, the collision meshes (behind the `meshes` feature), and the elbow singularity control-margin constants. Pure data: no kinematics or solver deps, so any consumer (`srs_model`, a viz tool, a sim bridge) builds from `urdf()` itself |
| [sim_bridge_core](./sim_bridge_core) | Rust | raw-to-peppygen pipelines for Isaac Sim and MuJoCo bridge nodes; the node supplies the peppylib transport |
| [sim_ext_core](./sim_ext_core) | Python | Plugin lifecycle, config loading, and sensor bridge plugins for the in-sim extensions; the node supplies the IO transport |
| [sim_robot_core](./sim_robot_core) | Python | What a simulation engine node knows about the robots it stands, whatever its physics: the registry of standing robots, the four pairing slots and what each pair names, one entry per robot model with its checks, and the camera configuration. The MuJoCo and Isaac Sim nodes install it in their base image |
| [control_core](./control_core) | Rust | Shared control-loop primitives for the openarm control nodes: a fixed-rate `Pacer` with overrun accounting |
| [peppy-shared](./peppy-shared) | Rust + Python | Cargo workspace of public-facing Peppy crates: the `peppylib` control library and its Python bindings, plus the messaging, config, and core-node API crates they build on (see [below](#peppy-shared-crates)) |

### peppy-shared crates

A virtual Cargo workspace. Its crates sit at the bottom of the dependency graph, shared by the `peppy` workspace and `platform-backend`.

| Crate | Language | Purpose |
|---|---|---|
| [peppylib-rs](./peppy-shared/peppylib-rs) (`peppylib`) | Rust | The Peppy control library: messaging, core-node helpers, runtime, services, config, and types |
| [peppylib-py](./peppy-shared/peppylib-py) | Python | PyO3 bindings exposing the `peppylib` control library to Python; published to PyPI as `peppylib` |
| [peppy-messaging-interface](./peppy-shared/peppy-messaging-interface) (`pmi`) | Rust | Messaging transport interface: zenoh transport plus an in-process mock adapter, sessions, and org-id namespace routing |
| [peppy-config-model](./peppy-shared/peppy-config-model) | Rust | Parsing and validation of the shared Peppy config documents: the `peppy.json5` node config model, runtime configs shipped to nodes, codegen fingerprints, and schema tags |
| [core-node-api](./peppy-shared/core-node-api) | Rust | Shared API surface for talking to a core-node daemon: capnp request/response types, service-name constants, and response parsers |
| [peppy-mcp-catalog](./peppy-shared/peppy-mcp-catalog) | Rust | The `mcp_exposure/v1` document model, its validation against the contracts it names (the canonical `message_format` to JSON Schema mapping included), and the versioned exposure catalog that validation derives, shared by the `peppy` binary and the MCP server runtime |
| [peppy-mcp-runtime](./peppy-shared/peppy-mcp-runtime) | Rust | The MCP server runtime the `peppy` binary serves exposures with: one Streamable HTTP listener on `127.0.0.1` speaking MCP `2026-07-28`, built on the official `rmcp` SDK, with one endpoint per exposure at `/<name>/<tag>/mcp` serving that exposure's resources, tools, and action-backed tasks with freshness, rate, representation, size, confirmation, and deadline policies |
| [json5-pretty](./peppy-shared/json5-pretty) | Rust | Pretty-print a `Serialize` value as JSON5 with unquoted object keys |
| [config-test-support](./peppy-shared/config-test-support) | Rust | Test fixtures shared across the Peppy workspaces (scratch dirs, and git-repo / node-config-template fixtures behind a feature) |
| [build-helpers](./peppy-shared/build-helpers) | Rust | Generic build-script helpers shared across peppy crates |

## Using these libraries

Consumers outside the `peppy` repository name the repository; cargo finds a package by name anywhere inside it, so a Rust dependency carries no path. Rust nodes pin `sim_bridge_core` in `Cargo.toml`:

```toml
[dependencies]
sim_bridge_core = { git = "https://github.com/Peppy-bot/peppy", rev = "<commit>" }
```

Python consumers install `sim_ext_core` from a pinned commit, naming its directory:

```
sim_ext_core @ git+https://github.com/Peppy-bot/peppy.git@<commit>#subdirectory=public-peppy-libs/sim_ext_core
```

Pin a commit rather than a branch: node builds happen inside containers and should be reproducible. When a library changes, bump the pin in the consuming node and rebuild it.

The `peppy-shared` crates reach their consumers three ways: the `peppy` workspace depends on them by path (`public-peppy-libs/peppy-shared/<crate>`), `platform-backend` by git like any library above, and Python consumers install `peppylib` from PyPI.

See each library's README for the API.
