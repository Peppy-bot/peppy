#!/bin/sh
set -eu

# Build and publish a GitHub Release for peppy.
# Thin wrapper that delegates to the Python implementation via pixi.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
command -v pixi >/dev/null 2>&1 || { echo "error: 'pixi' is required (https://pixi.sh)" >&2; exit 1; }
exec pixi run --manifest-path "$SCRIPT_DIR/pixi.toml" build-release "$@"
