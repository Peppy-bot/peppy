#!/usr/bin/env bash
# Buildkite port of .github/workflows/tests.yml (job: test).
set -euo pipefail

# Give this run its own peppy data root so the reused agent host does not
# accumulate or collide on /tmp/.peppy across runs. Only the cargo invocations
# below opt in via env; the release-scripts step is left alone so
# scripts/install.sh keeps defaulting PEPPY_HOME to $HOME/.peppy.
PEPPY_RUN_HOME="$(mktemp -d "${TMPDIR:-/tmp}/peppy-home.XXXXXX")"
mkdir -p "${PEPPY_RUN_HOME}/tmpdir"

# Reclaim the per-run data root even when a test step fails so disk does not
# grow on every run (was the `if: always()` cleanup step). The persistent
# build-tool cache at $HOME/.peppy/tmp is intentionally left in place (see
# build_helpers::cache_dir).
trap 'rm -rf "${PEPPY_RUN_HOME}"' EXIT

isolated() {
  PEPPY_HOME="${PEPPY_RUN_HOME}" TMPDIR="${PEPPY_RUN_HOME}/tmpdir" "$@"
}

# Formatting needs no PEPPY_HOME, so it stays outside `isolated`. First because
# it is the cheapest check here and failing fast on it saves a long build.
echo "--- :rust: cargo fmt"
cargo fmt --all -- --check

echo "--- :rust: cargo test"
isolated cargo test --locked

echo "--- :rust: container e2e tests"
isolated cargo test --locked -p core-node --features container_e2e --test container_e2e

echo "--- :rust: documentation integration tests"
isolated cargo test --locked -p docs-integration-tests

# Proves the build consumed the committed lockfile rather than re-resolving it,
# so the tested dependency graph is the shipped one. Must stay after the cargo
# steps: placed before them it proves nothing.
echo "--- :lock: shipping lockfile is unchanged"
git diff --exit-code -- Cargo.lock

# Release scripts tests run for every PR into main, and otherwise only when
# scripts/ changed (was dorny/paths-filter). For PR builds, diff against the
# merge base with the target branch; for push builds, main is protected and
# advances one merge (or squash) at a time, so the first-parent diff of the
# head commit is the pushed change.
scripts_changed() {
  if [[ "${BUILDKITE_PULL_REQUEST:-false}" != "false" ]]; then
    git fetch -q origin "refs/heads/${BUILDKITE_PULL_REQUEST_BASE_BRANCH}"
    ! git diff --quiet "$(git merge-base FETCH_HEAD HEAD)" HEAD -- scripts/
  else
    ! git rev-parse -q --verify HEAD^ >/dev/null \
      || ! git diff --quiet HEAD^ HEAD -- scripts/
  fi
}

echo "--- :package: release scripts tests"
if [[ "${BUILDKITE_PULL_REQUEST:-false}" != "false" && "${BUILDKITE_PULL_REQUEST_BASE_BRANCH:-}" == "main" ]] \
  || scripts_changed; then
  ./scripts/run_tests.sh --all
else
  echo "skipped: not a PR into main and scripts/ is unchanged"
fi
