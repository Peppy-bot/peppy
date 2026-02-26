#!/usr/bin/env bash
set -euo pipefail

# Build a peppylib wheel for PyPI with the version from PEPPY_GIT_TAG.
#
# Maturin reads the wheel version from Cargo.toml (via `version.workspace = true`),
# so we temporarily override it with the release tag. The Rust runtime __version__
# is set separately via option_env!("PEPPY_GIT_TAG") in build.rs / lib.rs.
#
# Usage: PEPPY_GIT_TAG=1.2.3 ./scripts/create_wheel.sh

if [[ -z "${PEPPY_GIT_TAG:-}" ]]; then
    echo "Error: PEPPY_GIT_TAG must be set" >&2
    exit 1
fi

CARGO_TOML="Cargo.toml"

# Back up Cargo.toml and restore it on exit (even on failure)
cp "$CARGO_TOML" "$CARGO_TOML.bak"
trap 'mv "$CARGO_TOML.bak" "$CARGO_TOML"' EXIT

# Replace workspace-inherited version with the release version.
# Using a temp file for portable sed (macOS sed -i requires an extension argument).
sed "s/^version\.workspace = true$/version = \"$PEPPY_GIT_TAG\"/" "$CARGO_TOML.bak" > "$CARGO_TOML"

# Build the wheel — PEPPY_GIT_TAG is already in the environment,
# so build.rs will also embed it into the Rust binary via option_env!
# Wheels are written to ../../target/wheels/ (the workspace target directory).
WHEEL_DIR="../../target/wheels"
maturin build --release --out "$WHEEL_DIR"

# Print the absolute path of the built wheel
realpath "$WHEEL_DIR"/peppylib-"$PEPPY_GIT_TAG"-*.whl
