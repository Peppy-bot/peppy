"""The branches a release is cut from and aligns, and the release they last shipped.

A release is cut from `dev` and nothing else: the archives are built from the
`dev` tip, the release notes are committed on `dev`, and `main` is then
fast-forwarded to that commit through a refspec push, so the two branches
always agree on what has shipped.
"""

from __future__ import annotations

import httpx

from .cli import ReleaseError, console
from .github import RepoSlug, get_latest_release
from .repo import (
    fetch_remote_branches,
    fetch_tag,
    get_commit,
    get_current_branch,
    is_ancestor,
)

# A release is cut from RELEASE_BRANCH, and ALIGNED_BRANCH is fast-forwarded
# to it once the release is published.
GIT_REMOTE = "origin"
RELEASE_BRANCH = "dev"
ALIGNED_BRANCH = "main"


def _describe_release_branch_drift(local_commit: str, remote_commit: str) -> str:
    """Explain how the local release branch differs from its remote counterpart."""
    remote_ref = f"{GIT_REMOTE}/{RELEASE_BRANCH}"
    if is_ancestor(local_commit, remote_commit):
        return (
            f"'{RELEASE_BRANCH}' is behind {remote_ref}. "
            f"Run `git pull --ff-only` and retry."
        )
    if is_ancestor(remote_commit, local_commit):
        return (
            f"'{RELEASE_BRANCH}' has commits that are not on {remote_ref}. "
            f"Run `git push {GIT_REMOTE} {RELEASE_BRANCH}` and retry."
        )
    return (
        f"'{RELEASE_BRANCH}' and {remote_ref} have diverged. "
        f"Reconcile them and retry."
    )


def verify_release_branch_state() -> str:
    """Check the git state a release needs, before a stage does anything.

    A release publishes from the `dev` tip, commits the generated notes on
    `dev`, and fast-forwards `main` to that commit. All three are checked here
    so a branch problem stops the stage before it writes anything:

    - HEAD is on `dev`, so the release is never cut from another branch,
    - `dev` matches `origin/dev`, so the final push cannot be rejected,
    - `origin/main` is an ancestor of `dev`, so `main` can fast-forward.

    Returns the `dev` commit.
    """
    current_branch = get_current_branch()
    if current_branch != RELEASE_BRANCH:
        found = f"'{current_branch}'" if current_branch else "a detached commit"
        raise ReleaseError(
            f"releases are cut from '{RELEASE_BRANCH}' only, but HEAD is on {found}. "
            f"Run `git checkout {RELEASE_BRANCH}` and retry."
        )

    console.print(
        f"Checking '{RELEASE_BRANCH}' against {GIT_REMOTE} "
        f"(and that '{ALIGNED_BRANCH}' can fast-forward to it)..."
    )
    fetch_remote_branches(GIT_REMOTE, (RELEASE_BRANCH, ALIGNED_BRANCH))

    release_commit = get_commit("HEAD")
    remote_release_commit = get_commit(f"{GIT_REMOTE}/{RELEASE_BRANCH}")
    if release_commit != remote_release_commit:
        raise ReleaseError(
            _describe_release_branch_drift(release_commit, remote_release_commit)
        )

    remote_aligned_commit = get_commit(f"{GIT_REMOTE}/{ALIGNED_BRANCH}")
    if not is_ancestor(remote_aligned_commit, release_commit):
        raise ReleaseError(
            f"{GIT_REMOTE}/{ALIGNED_BRANCH} has commits that are not on "
            f"'{RELEASE_BRANCH}', so '{ALIGNED_BRANCH}' cannot fast-forward to it. "
            f"Merge {GIT_REMOTE}/{ALIGNED_BRANCH} into '{RELEASE_BRANCH}' and retry."
        )

    return release_commit


def latest_release_tag(client: httpx.Client, slug: RepoSlug) -> str | None:
    """The tag of the latest published release, fetched, or None without one.

    The last release can be newer than this checkout, whose clone holds only
    the tags that existed when it was made, so the tag is fetched before it
    is read.
    """
    latest = get_latest_release(client, slug)
    tag = latest.get("tag_name") if latest else None
    if not tag:
        return None
    fetch_tag(GIT_REMOTE, tag)
    return tag
