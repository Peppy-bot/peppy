#!/bin/sh
set -eu

# Run the release scripts test suite.
# Requires: pixi (https://pixi.sh)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
command -v pixi >/dev/null 2>&1 || { echo "error: 'pixi' is required (https://pixi.sh)" >&2; exit 1; }
exec pixi run --manifest-path "$SCRIPT_DIR/pixi.toml" test "$@"
