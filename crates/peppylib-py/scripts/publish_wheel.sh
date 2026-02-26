#!/usr/bin/env bash
set -euo pipefail

# Upload a peppylib wheel to PyPI.
#
# Expects the wheel to have been built first via create_wheel.sh.
#
# Required env vars:
#   PEPPY_GIT_TAG       — version tag used to locate the wheel
#   MATURIN_PYPI_TOKEN  — PyPI API token for authentication
#
# Usage: PEPPY_GIT_TAG=1.2.3 MATURIN_PYPI_TOKEN=pypi-... ./scripts/publish_wheel.sh

if [[ -z "${PEPPY_GIT_TAG:-}" ]]; then
    echo "Error: PEPPY_GIT_TAG must be set" >&2
    exit 1
fi

if [[ -z "${MATURIN_PYPI_TOKEN:-}" ]]; then
    echo "Error: MATURIN_PYPI_TOKEN must be set" >&2
    exit 1
fi

if [[ "${MATURIN_PYPI_TOKEN}" != pypi-* ]]; then
    echo "Error: MATURIN_PYPI_TOKEN must be a PyPI API token (starts with 'pypi-')" >&2
    exit 1
fi

WHEEL_DIR="../../target/wheels"
WHEEL_GLOB="$WHEEL_DIR/peppylib-${PEPPY_GIT_TAG}-*.whl"

# shellcheck disable=SC2086
if ! ls $WHEEL_GLOB >/dev/null 2>&1; then
    echo "Error: No wheel found matching peppylib-${PEPPY_GIT_TAG}-*.whl in target/wheels/" >&2
    echo "Run 'PEPPY_GIT_TAG=$PEPPY_GIT_TAG pixi run create-wheel' first." >&2
    exit 1
fi

# shellcheck disable=SC2086
maturin upload --non-interactive $WHEEL_GLOB
