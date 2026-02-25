#!/bin/sh
set -eu

# Run the release scripts test suite.
# Requires: uv (https://docs.astral.sh/uv/)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
command -v uv >/dev/null 2>&1 || { echo "error: 'uv' is required (https://docs.astral.sh/uv/)" >&2; exit 1; }
exec uv run --project "$SCRIPT_DIR" --group dev pytest "$SCRIPT_DIR/tests" "$@"
