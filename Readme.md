# Peppy

[![Tests](https://github.com/Peppy-bot/peppy/actions/workflows/tests.yml/badge.svg)](https://github.com/Peppy-bot/peppy/actions/workflows/tests.yml)

Peppy is a modern robotics middleware framework designed for robots. Similar to ROS 2, it provides a distributed communication layer for robotic systems with a big focus on ease of use and explicit configuration.

Full documentation lives at **[docs.peppy.bot](https://docs.peppy.bot)**.

## 🛠️ Installation

```sh
curl -fsSL https://peppy.bot/install.sh | sh
```

The installer puts the `peppy` CLI on your `PATH`, registers the background service that builds and supervises nodes (systemd on Linux, launchd on macOS), and configures the [Apptainer](https://apptainer.org/) container runtime used by container nodes. Check that both the CLI and the service are up with:

```sh
peppy info
```

Peppy runs on Linux (x86_64/aarch64, tested on Ubuntu 24.04, Fedora, and Arch Linux) and macOS (aarch64).

The [installation guide](https://docs.peppy.bot/guides/installation/) covers version pinning (`PEPPY_VERSION`), skipping the service install (`PEPPY_NO_SERVICE_INSTALL`), and managing the service afterwards.

## 🤖 Try it

The [quickstart](https://docs.peppy.bot/quickstart/) takes you from nothing installed to a simulated bimanual [OpenArm](https://openarm.dev/) you can drive from your browser, without cloning a repository or writing any code:

```sh
peppy stack launch openarm_v2_teleop_mujoco
```

That single command adds, builds, and starts every node in the launcher, in dependency order, under the background service.

## 🚀 Key Features

- **Real-time Communication**: Low-latency message passing between nodes thanks to [Zenoh](https://github.com/eclipse-zenoh/zenoh)
- **Three interface kinds**: [topics](https://docs.peppy.bot/advanced_guides/topics/) for pub/sub streams, [services](https://docs.peppy.bot/advanced_guides/services/) for request/response, and [actions](https://docs.peppy.bot/advanced_guides/actions/) for long-running goals with feedback and cancellation
- **Quality of Service**: Configurable reliability and performance settings
- **Language Agnostic**: Support for Python and Rust (more languages will be supported in the future with `C` being the next on the list)
- **Cross-platform**: Linux and macOS support
- **Supervised node stacks**: A background daemon builds, starts, health-checks, and restarts the nodes of a stack; [launch files](https://docs.peppy.bot/guides/launch_files/) declare a whole robot in one document
- **Container nodes**: Nodes can ship as [containers](https://docs.peppy.bot/advanced_guides/containers/) built and run on Apptainer, with no change to how they are wired
- **Node repositories**: Resolve nodes and launchers by name from [repositories](https://docs.peppy.bot/advanced_guides/repositories/) you or others publish
- **Explicit over implicit**: Every node communication or feature of the framework is controlled through explicit configuration. This allows things like output messages of one node to break the code of other nodes depending on it to avoid implicit crashes at runtime.
- **Not opinionated on build tools**: Peppy doesn't force any tool upon its developers. The `peppylib` library however, is available only in a few supported languages.

Non-goals:

- We do _not_ aim for API stability between releases until 1.0, preferring to iterate quickly and refine the API as much as possible. But we do [follow SemVer](https://doc.rust-lang.org/cargo/reference/semver.html).

## 🧰 The CLI

| Command group | What it does |
|---|---|
| `peppy node` | Scaffold, add, build, run, inspect, and stop individual nodes |
| `peppy stack` | Launch a stack from a launcher, list what is running, benchmark interface latency |
| `peppy repo` | Manage the repositories nodes and launchers are resolved from |
| `peppy container` | Check and repair the Apptainer container prerequisites |
| `peppy platform` | Log in, log out, and show the current platform identity |
| `peppy service` | Install, serve, stop, uninstall, and reset the background service |
| `peppy info` | Print the CLI version, container setup, and daemon info |

## 📚 Documentation

- [Quickstart](https://docs.peppy.bot/quickstart/): drive a simulated bimanual robot in three steps
- [Creating your first node](https://docs.peppy.bot/guides/first_node/): write and run a node from scratch, in Rust or Python
- [Communication](https://docs.peppy.bot/guides/communication/) and [choosing a pattern](https://docs.peppy.bot/advanced_guides/communication_patterns/): wiring nodes together
- [Concepts](https://docs.peppy.bot/reference/concepts/): nodes, instances, contracts, and the stack, defined
- [Daemon configuration](https://docs.peppy.bot/advanced_guides/daemon_config/): the `peppy_config.json5` reference, including running your own Zenoh router instead of the one Peppy manages
- [Changelog](https://docs.peppy.bot/reference/changelog/): release notes

LLM-friendly versions of the documentation are available at [`/llms.txt`](https://docs.peppy.bot/llms.txt) and [`/llms-full.txt`](https://docs.peppy.bot/llms-full.txt).

## 🔨 Building from source

You only need this to work on Peppy itself. To use Peppy, install it with the command above.

### New machine setup

To provision a fresh development machine, run:

```
./scripts/setup_machine.sh
```

It installs the following tooling with each project's recommended method, skipping anything already present:

- **qemu** (`apt` on Ubuntu, Homebrew on macOS)
- **Go** (official go.dev tarball into `/usr/local/go` on Ubuntu, Homebrew on macOS)
- **pixi** (official installer)
- **uv** (official installer)
- **Lima** (Homebrew, macOS only)

Supported platforms are Ubuntu and macOS. On macOS the script expects [Homebrew](https://brew.sh) to be installed first. When Go is installed from the tarball on Ubuntu, the script adds `/usr/local/go/bin` to your `~/.profile`, so open a new shell (or `source ~/.profile`) afterwards.

### Build

Install [cargo & Rust](https://doc.rust-lang.org/cargo/getting-started/installation.html) then make sure the Rust toolchain is up to date with:
```
rustup update
```
To build Peppy, just type the following:
```
cargo build --release --all-targets
```

The public-facing crates (`config`, `peppylib`, `pmi`, and friends) are git dependencies on [`public-peppy-libs`](https://github.com/Peppy-bot/public-peppy-libs), resolved through `Cargo.lock`. Nothing extra to clone; `cargo update` moves them forward.

### Test

```
cargo test --locked
```

The workspace's `default-members` covers `crates/*` only, so the slow documentation integration tests are excluded from that run. Run them explicitly with:

```
cargo test -p docs-integration-tests
```

CI runs both, plus the feature-gated container and multi-daemon end-to-end suites and the release-scripts tests. See [`.github/workflows/tests.yml`](.github/workflows/tests.yml) for the exact commands.

## 📄 License

Peppy is licensed under the [Business Source License 1.1](LICENSE). You may make production use of it, provided that use does not include offering a product or service to third parties whose value derives primarily from Peppy. On 2031-01-01 the license converts to Apache License, Version 2.0.
