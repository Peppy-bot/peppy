# peppylib

Python bindings for the peppyOS control library.

## Prerequisites

- Python >= 3.11
- Rust toolchain (install via [rustup](https://rustup.rs/))
- [Pixi](https://pixi.sh/) package manager
- On macOS: Xcode Command Line Tools (`xcode-select --install`)

## Development

### Setup

```bash
cd crates/peppylib-py
pixi install
```

### Build and run examples

Build the native extension and run an example with a single command:

```bash
# Run the topic publisher example
pixi run run-topics-exposes

# Run the topic subscriber example
pixi run run-topics-subscribes
```

### Build only

To build the native extension without running an example:

```bash
pixi run dev
```

### Run tests

```bash
pixi run test
```
