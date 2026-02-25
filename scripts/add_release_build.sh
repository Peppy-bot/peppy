#!/bin/sh
set -eu

# Build peppy for the current host and upload it to an existing GitHub Release.
# Thin wrapper that delegates to the Python implementation via uv.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
command -v uv >/dev/null 2>&1 || { echo "error: 'uv' is required (https://docs.astral.sh/uv/)" >&2; exit 1; }
exec uv run --project "$SCRIPT_DIR" add-release-build "$@"
