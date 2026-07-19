#!/usr/bin/env bash
set -euo pipefail

# Exercise the federation control path with debug_assertions disabled.  When a
# sibling public-peppy-libs checkout is supplied, Cargo is patched explicitly to
# that checkout so a coordinated release set is tested before either repository
# has to be published.

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
public_libs=""

if [[ $# -gt 0 ]]; then
  if [[ $# -ne 2 || $1 != "--local-public-libs" ]]; then
    echo "usage: ${BASH_SOURCE[0]##*/} [--local-public-libs PATH]" >&2
    exit 2
  fi
  public_libs="$(cd "$2" && pwd)"
fi

cargo_config=()
if [[ -n $public_libs ]]; then
  shared="$public_libs/peppy-shared"
  shared_crates=(
    "pmi|$shared/peppy-messaging-interface"
    "peppy-config-model|$shared/peppy-config-model"
    "peppylib-rs|$shared/peppylib-rs"
    "core-node-api|$shared/core-node-api"
    "json5-pretty|$shared/json5-pretty"
    "config-test-support|$shared/config-test-support"
    "build-helpers|$shared/build-helpers"
  )

  for shared_crate in "${shared_crates[@]}"; do
    package=${shared_crate%%|*}
    crate_path=${shared_crate#*|}
    manifest="$crate_path/Cargo.toml"
    [[ -f $manifest ]] || {
      echo "release federation check failed: missing local shared crate $manifest" >&2
      exit 1
    }
    cargo_config+=(
      --config
      "patch.\"https://github.com/Peppy-bot/public-peppy-libs\".$package.path=\"$crate_path\""
    )
  done
fi

cargo_gate() {
  cargo "${cargo_config[@]}" "$@"
}

if [[ -n $public_libs ]]; then
  metadata="$(cd "$repository_root" && cargo_gate metadata --locked --format-version 1)"
  expected="\"manifest_path\":\"$public_libs/peppy-shared/peppy-messaging-interface/Cargo.toml\""
  if [[ $metadata != *"$expected"* ]]; then
    echo "release federation check failed: Peppy did not resolve pmi from $public_libs" >&2
    exit 1
  fi
  echo "release federation dependency: local public-peppy-libs checkout"
else
  echo "release federation dependency: Cargo.lock source"
fi

cd "$repository_root"

# These suites contain the certificate lifecycle, production target resolver,
# daemon renewal/refederation state machine, CLI status, and breaking v3 storage
# guards.  `--release` is essential: the shared development identity is selected
# with cfg(debug_assertions) and must not make this path pass.
cargo_gate fmt --all -- --check
cargo_gate test --release --locked -p auth --lib
cargo_gate test --release --locked -p daemon --lib router_federation
cargo_gate test --release --locked -p peppy --lib commands::platform
cargo_gate test --release --locked -p peppy --test integration platform_flow
cargo_gate clippy --release --locked -p auth -p daemon -p peppy --all-targets -- -D warnings
cargo_gate build --release --locked -p peppy

# Artifact-level guard: neither committed shared development client credential
# may be embedded in the shipped binary.  Use a full base64 payload line rather
# than a friendly label, which makes this check sensitive to the actual fixture.
binary="${CARGO_TARGET_DIR:-$repository_root/target}/release/peppy"
[[ -x $binary ]] || {
  echo "release federation check failed: release binary not found at $binary" >&2
  exit 1
}

if ! LC_ALL=C grep -aFq -- 'https://api.peppy.bot' "$binary"; then
  echo "release federation check failed: shipped binary lacks the production API origin" >&2
  exit 1
fi
for debug_origin in 'http://127.0.0.1:3000' 'http://auth.peppy.localhost:8080'; do
  if LC_ALL=C grep -aFq -- "$debug_origin" "$binary"; then
    echo "release federation check failed: shipped binary embeds debug origin $debug_origin" >&2
    exit 1
  fi
done

reject_fixture_payload() {
  fixture=$1
  description=$2
  needle=$(awk 'length($0) >= 48 && $0 !~ /^-----/ { print; exit }' "$fixture")
  [[ -n $needle ]] || {
    echo "release federation check failed: could not read $description fixture" >&2
    exit 1
  }
  if LC_ALL=C grep -aFq -- "$needle" "$binary"; then
    echo "release federation check failed: shipped binary embeds $description" >&2
    exit 1
  fi
}

reject_fixture_payload \
  "$repository_root/crates/auth-internal/dev-ca/peppy-dev-client.pem" \
  "the shared development client certificate"
reject_fixture_payload \
  "$repository_root/crates/auth-internal/dev-ca/peppy-dev-client-key.pem" \
  "the shared development client private key"
reject_fixture_payload \
  "$repository_root/crates/auth-internal/dev-ca/peppy-dev-ca.pem" \
  "the shared development CA"

echo "Peppy release federation gate: ok"
