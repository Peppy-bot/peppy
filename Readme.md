# Peppy

[![Tests](https://github.com/Peppy-bot/peppy/actions/workflows/tests.yml/badge.svg?branch=main)](https://github.com/Peppy-bot/peppy/actions/workflows/tests.yml?query=branch%3Amain)

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
peppy stack launch openarm_v2 --with=mujoco,web_commander
```

That single command adds, builds, and starts every node in the selection, in dependency order, under the background service.

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
| `peppy stack` | Launch or build a stack from a launcher, list what is running, benchmark interface latency |
| `peppy repo` | Manage the repositories nodes and launchers are resolved from |
| `peppy container` | Check and repair the Apptainer container prerequisites |
| `peppy platform` | Log in, log out, and show the current platform identity |
| `peppy service` | Install, serve, stop, uninstall, and reset the background service |
| `peppy info` | Print the CLI version, container setup, and daemon info |
| `peppy --version` | Print the CLI version alone, without contacting the daemon |

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

Peppy builds apptainer and its bundled squashfuse from source, so the host needs their build dependencies. On Ubuntu:

```
sudo apt-get install -y make gcc g++ pkg-config squashfs-tools cryptsetup curl ca-certificates \
  libseccomp-dev libfuse3-dev zlib1g-dev liblzo2-dev liblz4-dev liblzma-dev libzstd-dev fuse2fs uidmap
```

That list mirrors `APPTAINER_BUILD_DEPS` in [`crates/containers-internal/build.rs`](./crates/containers-internal/build.rs), which the build script asserts before it starts, plus three the constant does not carry: `fuse2fs`, which apptainer needs at run time to mount EXT3 images; `uidmap`, which provides the `newuidmap` that fakeroot needs; and `g++`, which apptainer's `mconfig` probes for and which Ubuntu's `gcc` package does not pull in. macOS builds apptainer inside Lima instead, so the host needs none of them.

Then the toolchains, each from its own project's recommended installer:

- **Rust, with clippy** ([rustup](https://rustup.rs)). Track current stable: the dependency graph moves with it. clippy is a test dependency rather than a lint here, because the generator suites run `cargo clippy --all-targets -- -D warnings` over the crates they generate.
- **Go** ([go.dev](https://go.dev/dl/)). `containers-internal` compiles apptainer with the `go` it finds on PATH, and apptainer's `mconfig` enforces a minimum of its own that Ubuntu's `golang-go` does not meet.
- **pixi** ([pixi.sh](https://pixi.sh)), at or above the `requires-pixi` in [`scripts/pixi.toml`](./scripts/pixi.toml). `generator-internal`'s build script builds the embedded peppylib `.so` through a bare `pixi run`.
- **uv** ([astral.sh/uv](https://astral.sh/uv)). The Python generator tests and the release scripts exec a bare `uv`.
- **qemu** (`apt`, or Homebrew with **Lima** on macOS). The release-script tests boot Lima VMs.
- **Docker, with buildx** (`apt` on Ubuntu; on macOS choose your own runtime). The multi-daemon suite builds its daemon image with `docker buildx build`.
- **Node.js**, only for the docs site in [`docs/`](./docs); its `package.json` names the major it wants.

When Go is installed from the tarball, add `/usr/local/go/bin` to your `~/.profile`. Docker group membership only applies to new logins, so log out and back in before running the multi-daemon suite.

### Self-hosted CI runners

The workflows install nothing. They use the toolchain the box was provisioned with, and the first step of every job stops the run naming anything missing rather than installing it, so a box is prepared once and then simply takes work. Provisioning happens outside this repository; it installs everything listed above, and two things beyond it:

- **passwordless sudo** for the user the Actions runner executes as. Each container suite installs a peppy release under the job's own directory and runs `peppy container setup`, which writes an AppArmor profile keyed to that install's path, and a job has no terminal for `sudo` to prompt at.
- **the aarch64 cross toolchain** (`rustup target add aarch64-unknown-linux-gnu` and `gcc-aarch64-linux-gnu`) that the cross-check job builds with.

Install the toolchains **as the runner's own user**, not `root` and not through `sudo`: rustup, pixi and uv install into `$HOME`, and a run under `sudo` puts them where the runner will never look. Two steps afterwards, both needed:

```
# 1. docker group membership and anything new on PATH only reach new sessions
sudo systemctl restart 'actions.runner.*'

# 2. the runner service does not source ~/.profile, so name the toolchains in
#    its own environment file, <runner-dir>/.env, then restart it again:
PATH=$HOME/.cargo/bin:/usr/local/go/bin:$HOME/.pixi/bin:$HOME/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
```

A box that has not had this done is not a neutral member of the pool: every job targets a bare `self-hosted` label, so an unprepared runner takes work it cannot complete and fails it. Provision first, then register.

### Build

Install [cargo & Rust](https://doc.rust-lang.org/cargo/getting-started/installation.html) if you have not already, then keep the toolchain current with:
```
rustup update
```
To build Peppy, just type the following:
```
cargo build --release --all-targets
```

The public-facing crates (`config`, `peppylib`, `pmi`, and friends) live in [`peppy-shared/`](peppy-shared/), a sealed tree this workspace depends on by path. The tree is a workspace of its own, excluded from this one, and the dependency runs one way only: nothing in it may depend on `crates/`, which is what lets `platform-backend` consume it on its own. [`peppy-shared/build-helpers/tests/sealed_tree.rs`](peppy-shared/build-helpers/tests/sealed_tree.rs) holds every manifest and symlink in the tree to that rule, and CI runs the tree's suites from a checkout holding nothing but the tree, so a reach a manifest cannot show has nothing to resolve to.

### Test

```
cargo test --locked
```

The workspace's `default-members` covers `crates/*` only, so the slow documentation integration tests are excluded from that run. Run them explicitly with:

```
cargo test -p docs-integration-tests
```

The `peppy-shared` tree holds a workspace of its own, so its suites run from there:

```
cd peppy-shared
cargo test --workspace --exclude peppylib-py --locked
```

CI runs all of these, plus the feature-gated container and multi-daemon end-to-end suites and the release-scripts tests. See [`.github/workflows/tests.yml`](.github/workflows/tests.yml) for the exact commands.

## 📄 License

Peppy is licensed under the [Business Source License 1.1](LICENSE). You may make production use of it, provided that use does not include offering a product or service to third parties whose value derives primarily from Peppy. On 2031-01-01 the license converts to Apache License, Version 2.0.

The [`peppy-shared/`](peppy-shared/) directory is licensed separately, under the [Apache License, Version 2.0](peppy-shared/LICENSE).
