#!/bin/sh
set -eu

# Run the release scripts test suite.
# Requires: pixi (https://pixi.sh)
#
# Arguments are passed through to pytest, so any selection it understands
# works here:
#
#   ./run_tests.sh                  every test
#   ./run_tests.sh -m 'not install' the mocked tests alone (about a second)
#   ./run_tests.sh -m install       the install.sh tests alone
#   ./run_tests.sh -k reinstall     by name
#
# The install.sh tests change the machine they run on, so they skip unless
# PEPPY_DISPOSABLE_TEST_HOST=1 declares the machine disposable.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
command -v pixi >/dev/null 2>&1 || { echo "error: 'pixi' is required (https://pixi.sh)" >&2; exit 1; }
exec pixi run --manifest-path "$SCRIPT_DIR/pixi.toml" test "$@"
