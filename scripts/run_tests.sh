#!/bin/sh
set -eu

# Run the release scripts test suite.
# Requires: pixi (https://pixi.sh)
#
# Arguments are passed through to pytest, so any selection it understands
# works here:
#
#   ./run_tests.sh                  every test, native arch
#   ./run_tests.sh -m 'not vm'      the mocked tests alone (about a second)
#   ./run_tests.sh -m vm            the Lima install tests alone
#   ./run_tests.sh --cross-arch     add the cross-arch guests (macOS only)
#   ./run_tests.sh -k install       by name
#
# This script used to exec a fixed `test-all` task and drop its arguments on
# the floor, so `run_tests.sh --all` ran neither what it named nor what the
# caller meant -- CI passed that flag for months believing it bought
# cross-arch coverage it cannot have on Linux.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
command -v pixi >/dev/null 2>&1 || { echo "error: 'pixi' is required (https://pixi.sh)" >&2; exit 1; }
exec pixi run --manifest-path "$SCRIPT_DIR/pixi.toml" test "$@"
