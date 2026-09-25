#!/usr/bin/env bash
# Loads the deploy key of private-nodes-hub, given in PRIVATE_NODES_HUB_DEPLOY_KEY,
# into an ssh-agent of this job.
#
# peppy authenticates ssh clones through the ssh-agent, so the agent's socket
# is exported through GITHUB_ENV: every later step of the job reads
# private-nodes-hub through it, `git ls-remote` and `git fetch` as well as the
# daemon a later step starts. The agent is this job's own and dies with the
# runner.
#
# github.com's host key is pinned, not trusted on first sight: the scan must
# fingerprint exactly GitHub's published ed25519 key, or the job stops before
# any private bytes move. It lands in ~/.ssh/known_hosts, where libgit2,
# peppy's git transport, checks it.
#
# The hub-ci-peppy action runs this script through $GITHUB_ACTION_PATH, and the
# hub-set job of the peppy release, which reads private-nodes-hub, runs it from
# its checkout. It is a script because a composite action that another
# repository calls cannot use a local action beside it, and both have to run
# it.
set -euo pipefail

if [ -z "${PRIVATE_NODES_HUB_DEPLOY_KEY:-}" ]; then
  echo "PRIVATE_NODES_HUB_DEPLOY_KEY is empty: give this step the deploy key of private-nodes-hub" >&2
  exit 1
fi

mkdir -p ~/.ssh && chmod 700 ~/.ssh
scan="$(ssh-keyscan -t ed25519 github.com 2>/dev/null)"
fingerprint="$(ssh-keygen -lf - <<<"$scan" | awk '{print $2}')"
if [ "$fingerprint" != "SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU" ]; then
  echo "github.com fingerprints as $fingerprint, not GitHub's published ed25519 key" >&2
  exit 1
fi
printf '%s\n' "$scan" >>~/.ssh/known_hosts

eval "$(ssh-agent -a "$RUNNER_TEMP/private-hub-agent.sock")" >/dev/null
key="$(mktemp "$RUNNER_TEMP/private-hub-key.XXXXXX")"
printf '%s\n' "${PRIVATE_NODES_HUB_DEPLOY_KEY%$'\n'}" >"$key"
chmod 600 "$key"
ssh-add "$key"
rm -f "$key"
echo "SSH_AUTH_SOCK=$SSH_AUTH_SOCK" >>"$GITHUB_ENV"
