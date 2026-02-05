# peppylib

Python bindings for the peppyOS control library.

## Prerequisites

- Python >= 3.11
- Rust toolchain (install via [rustup](https://rustup.rs/))
- On macOS: Xcode Command Line Tools (`xcode-select --install`)

## Development

### Setup

```bash
cd crates/peppylib-py
```

### Build and run examples

Build the native extension and run an example with a single command:

```bash
# Run the topic publisher example
uv run --group dev task run-topics-exposes

# Run the topic subscriber example
uv run --group dev task run-topics-subscribes
```

### Build only

To build the native extension without running an example:

```bash
uv run --group dev task dev
```