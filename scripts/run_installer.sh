#!/usr/bin/env bash
#
# Runs the install script at a URL the way `curl -fsSL <url> | bash` does, and
# runs it again when it fails.
#
# An installer such as that of pixi or of the Claude CLI downloads what it
# installs with no retry of its own, so one transient failure of the host it
# downloads from (GitHub answering with a gateway timeout, for one) fails the
# whole install. An attempt that fails is followed by a wait that grows by
# INSTALLER_RETRY_WAIT seconds each time, and the attempts cover about three
# minutes, as the retries of every other download of a release do (see
# scripts/functions/download.py).
#
# The installer gets the environment of this script, and the arguments that
# follow the URL:
#
#   PIXI_VERSION=v0.80.0 scripts/run_installer.sh https://pixi.sh/install.sh
set -euo pipefail

INSTALLER_ATTEMPTS=4
INSTALLER_RETRY_WAIT=30

if [ "$#" -lt 1 ]; then
  echo "usage: $0 <installer URL> [installer arguments...]" >&2
  exit 2
fi
url="$1"
shift

attempt=1
until curl -fsSL "$url" | bash -s -- "$@"; do
  if [ "$attempt" -ge "$INSTALLER_ATTEMPTS" ]; then
    echo "error: the installer at $url failed $INSTALLER_ATTEMPTS times" >&2
    exit 1
  fi
  wait_seconds=$((attempt * INSTALLER_RETRY_WAIT))
  echo "The installer at $url failed (attempt $attempt of $INSTALLER_ATTEMPTS); trying again in ${wait_seconds}s" >&2
  sleep "$wait_seconds"
  attempt=$((attempt + 1))
done
