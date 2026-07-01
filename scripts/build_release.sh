#!/bin/sh
set -eu

# Build and publish a GitHub Release for peppy.
# Thin wrapper that delegates to the Python implementation via pixi.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
command -v pixi >/dev/null 2>&1 || { echo "error: 'pixi' is required (https://pixi.sh)" >&2; exit 1; }

# Enable the release-only Linux container bindings: cross-compile the peppylib
# .so for every Linux arch (linux-aarch64 and linux-x86_64) from this macOS host.
# A plain `cargo build` leaves this unset and, like a Linux build, produces only
# the host dynamic lib, so local dev builds stay fast; only this script and CI
# set it. Native apptainer is always built regardless of this flag; cross-arch
# apptainer is a separate axis, gated by PEPPY_CROSS_ARCH (set in
# scripts/functions/build.py).
export PEPPY_CROSS_BUILD=1

# Force a fresh peppylib native-extension build (including the Linux cross-compile)
# so release artifacts never embed a stale .so left over from a debug build.
export PEPPYLIB_REBUILD=1

exec pixi run --manifest-path "$SCRIPT_DIR/pixi.toml" build-release "$@"
