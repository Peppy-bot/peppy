#!/bin/sh
set -eu

# Build and publish a GitHub Release for peppy.
# Thin wrapper that delegates to the Python implementation via pixi.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
command -v pixi >/dev/null 2>&1 || { echo "error: 'pixi' is required (https://pixi.sh)" >&2; exit 1; }

# The Linux container bindings (the peppylib .so for linux-aarch64 and
# linux-x86_64) need no flag here: every release build cross-compiles them, and
# this script builds everything in release. Cross-arch apptainer is a separate
# axis, gated by PEPPY_CROSS_ARCH (set in scripts/functions/build.py).

# Force a fresh peppylib native-extension build (including the Linux cross-compile)
# so release artifacts never embed a stale .so left over from a debug build.
export PEPPYLIB_REBUILD=1

exec pixi run --manifest-path "$SCRIPT_DIR/pixi.toml" build-release "$@"
