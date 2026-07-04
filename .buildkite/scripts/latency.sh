#!/usr/bin/env bash
# Buildkite port of .github/workflows/latency.yml.
#
# Runs only for the release PR that merges dev into main (gated by the step
# `if` in pipeline.yml), so routine PRs into dev never trigger these heavy
# real-node tests. The latency tests are #[ignore] by default, so the plain
# `cargo test` in tests.sh never runs them — they run only here, via --ignored.
set -euo pipefail

# Give this run its own peppy data root (shared_target_dir lives under it) so
# the reused agent host does not accumulate or collide on /tmp/.peppy across
# runs. Reclaimed on exit even when the tests fail.
PEPPY_RUN_HOME="$(mktemp -d "${TMPDIR:-/tmp}/peppy-home.XXXXXX")"
mkdir -p "${PEPPY_RUN_HOME}/tmpdir"
trap 'rm -rf "${PEPPY_RUN_HOME}"' EXIT

# The guard asserts the MEDIAN against generous ceilings, so it does not flake
# on ordinary jitter. The *absolute* numbers, however, swing with CPU turbo
# frequency and shared-host load — both environmental, not code. To get tight,
# trustworthy numbers (and to allow tighter ceilings), set up the agent host
# for a stable clock (needs root; the test does not do this itself):
#   - performance governor:  cpupower frequency-set -g performance
#   - disable turbo:         echo 1 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo
#   - reserve cores:         boot with isolcpus=<cores>, then run the test under
#                            taskset -c <cores> on an otherwise-idle box
# The Python responder scenarios need `uv`; set PEPPY_LATENCY_SKIP_PYTHON=1 on
# the agent (or on the step in pipeline.yml) to run only the Rust scenarios if
# it is absent.
echo "--- :stopwatch: roundtrip latency threshold tests (real nodes)"
export PEPPY_HOME="${PEPPY_RUN_HOME}" TMPDIR="${PEPPY_RUN_HOME}/tmpdir"
cargo test -p core-node --test latency -- --ignored --test-threads 1 --nocapture
