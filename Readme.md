# Peppy Operating system

[![Tests](https://github.com/Peppy-bot/peppy/actions/workflows/tests.yml/badge.svg)](https://github.com/Peppy-bot/peppy/actions/workflows/tests.yml)

PeppyOS is a modern robotics middleware framework designed for robots. Similar to ROS 2, it provides a distributed communication layer for robotic systems with a big focus on ease of use and explicit configuration.

## 🚀 Key Features

- **Real-time Communication**: Low-latency message passing between nodes thanks to [Zenoh](https://github.com/eclipse-zenoh/zenoh)
- **Quality of Service**: Configurable reliability and performance settings
- **Language Agnostic**: Support for Python and Rust (more languages will be supported in the future with `C` being the next on the list)
- **Cross-platform**: Linux and macOS support
- **Explicit over implicit**: Every node communication or feature of the framework is controlled through explicit configuration. This allows things like output messages of one node to break the code of other nodes depending on it to avoid implicit crashes at runtime.
- **Not opinionated on build tools**: PeppyOS doesn't force any tool upon it's developers. The `peppylib` library however, is available only in a few supported languages.

Non-goals:

- We do _not_ aim for API stability between releases until 1.0, preferring to iterate quickly and refine the API as much as possible. But we do [follow SemVer](https://doc.rust-lang.org/cargo/reference/semver.html).

## 📋 Table of Contents

- [Installation](#installation)
- [Examples](#examples)

## 🛠️ Installation

### Prerequisites

Install [cargo & Rust](https://doc.rust-lang.org/cargo/getting-started/installation.html) then make sure the Rust toolchain is up to date with:
```
rustup update
```
To build PeppyOS, just type the following:
```
cargo build --release --all-targets
```

Note: `peppy service serve` uses Zenoh and spawns a `zenohd` router process. If `zenohd` isn't next to the `peppy` binary or on `PATH`, set `PEPPY_ZENOHD_PATH` to its location.


## 📚 Examples

Explore the `examples/` directory for comprehensive demonstrations:

- **Basic Communication**: Simple publisher/subscriber examples
