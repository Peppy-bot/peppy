#!/usr/bin/env bash
set -e

SECONDS=0
print_total_time() {
  local elapsed=$SECONDS
  local hours=$((elapsed / 3600))
  local minutes=$(((elapsed % 3600) / 60))
  local seconds=$((elapsed % 60))

  if ((hours > 0)); then
    printf 'Total time: %dh%02dm%02ds\n' "$hours" "$minutes" "$seconds" >&2
  else
    printf 'Total time: %dm%02ds\n' "$minutes" "$seconds" >&2
  fi
}
trap print_total_time EXIT

# Ensure sccache is available before using it as RUSTC_WRAPPER.
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
if ! command -v sccache >/dev/null 2>&1; then
  echo "sccache not found; installing via cargo..." >&2
  cargo install sccache --locked
fi

export RUSTC_WRAPPER=sccache
cargo clean && cargo test
