#!/bin/sh
set -eu

# Run one stage of a peppy release built on several machines at once (see
# .github/workflows/parallel-release.yml and scripts/functions/parallel_release.py).
# Thin wrapper that delegates to the Python implementation via pixi.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
command -v pixi >/dev/null 2>&1 || { echo "error: 'pixi' is required (https://pixi.sh)" >&2; exit 1; }

exec pixi run --manifest-path "$SCRIPT_DIR/pixi.toml" parallel-release "$@"
