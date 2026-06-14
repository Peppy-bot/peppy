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

### Build

To build the native extension:

```bash
pixi run dev
```

### Run tests

```bash
pixi run test
```
