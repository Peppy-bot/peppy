# Peppy Operating system

[![Tests](https://github.com/ekami/peppy/actions/workflows/tests.yml/badge.svg)](https://github.com/ekami/peppy/actions/workflows/tests.yml)

PeppyOS is a modern robotics middleware framework designed for robots. Similar to ROS 2, it provides a distributed communication layer for robotic systems with a big focus on ease of use and explicit configuration.

## 🚀 Key Features

- **Real-time Communication**: Low-latency message passing between nodes thanks to [Zenoh](https://github.com/eclipse-zenoh/zenoh)
- **Quality of Service**: Configurable reliability and performance settings
- **Language Agnostic**: Support for Python and Rust (more languages will be supported in the future including [Mojo](https://www.modular.com/mojo))
- **Cross-platform**: Linux, Windows, and macOS support

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


## 📚 Examples

Explore the `examples/` directory for comprehensive demonstrations:

- **Basic Communication**: Simple publisher/subscriber examples

