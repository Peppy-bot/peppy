# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Common Development Commands

### Build Commands
```bash
# Build the project (release mode)
cargo build --release --all-targets

# Build in development mode
cargo build

# Clean build artifacts
cargo clean
```

### Test Commands
```bash
# Run all tests
cargo test

# Run a specific test
cargo test test_name

# Run tests with output
cargo test -- --nocapture

# Run tests in a specific module
cargo test node::
```

### Linting and Formatting
```bash
# Format code
cargo fmt

# Check formatting without making changes
cargo fmt --check

# Run clippy linter
cargo clippy

# Run clippy with all targets
cargo clippy --all-targets
```

### Running the Application
```bash
# Run the main binary
cargo run

# Run with specific subcommands
cargo run -- node create my-node
cargo run -- serve --name my-service
cargo run -- sync peppy.star
cargo run -- pixi install
```

## High-Level Architecture

PeppyOS is a robotics middleware framework similar to ROS 2, built in Rust with Python bindings. The architecture consists of:

### Core Components

1. **Node System** (`src/node/`): The fundamental building blocks of PeppyOS
   - Nodes are distributed components that communicate via Zenoh
   - Each node has its own pixi environment and peppy.star configuration
   - Node creation sets up directory structure with pixi.toml and peppy.star files

2. **CLI Interface** (`src/main.rs`): Entry point providing subcommands:
   - `node create`: Creates new nodes with scaffolding
   - `serve`: Runs the peppy service/Zenoh router
   - `pixi`: Proxies commands to the pixi package manager
   - `sync`: Synchronizes peppy.star configuration files

3. **Communication Layer**: Built on Zenoh for real-time, distributed messaging
   - Provides low-latency message passing between nodes
   - Configurable QoS settings for reliability and performance

4. **Configuration System**: Uses Starlark (`.star` files) for node configuration
   - Each node has a `peppy.star` file defining its behavior
   - Configuration is evaluated using the starlark interpreter

5. **Language Bindings**: PyO3 integration for Python support
   - Allows nodes to be written in both Rust and Python
   - Future support planned for additional languages like Mojo

### Key Dependencies

- **zenoh**: Distributed communication framework
- **starlark**: Configuration language interpreter
- **pyo3**: Python bindings
- **capnp**: Serialization (similar to ROS 2's CDR)
- **tokio**: Async runtime
- **clap**: CLI argument parsing

### Code Style and Best Practices

- **Modular Design**: Keep files under 500 lines, optimize with parallel analysis
- **Environment Safety**: Never hardcode secrets, validate with concurrent checks
- **Test-First**: Always write tests before implementation
- **Clean Architecture**: Separate concerns with concurrent validation
- **Parallel Documentation**: Maintain clear, up-to-date documentation with concurrent updates
- Minimize the use of `.clone()` as much as possible
- Strictly follow the instructions that are given, do not attempt to modify code outside the scope of what is asked
- Make use of `RefCell` only when strictly necessary (in unit and integration tests during mocking for instance)

### Testing Strategy

Tests are organized under `tests/` with:
- Integration tests for node creation and management
- Helper utilities in `tests/helpers/mod.rs` for test setup

When working on this codebase, ensure all tests pass before committing changes.
