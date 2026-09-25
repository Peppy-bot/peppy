"""The branches a release is cut from and aligns, and the release they last shipped.

A release is cut from `dev` and nothing else: the archives are built from the
`dev` tip the release starts from, the release commit. Once the release is
published, its notes are committed on top of the release commit and merged
into `dev`, which may have taken merges since, and `main` is fast-forwarded to
the notes commit through a refspec push. `main` then holds exactly what has
shipped, and `dev` holds it too.
"""

from __future__ import annotations

import httpx

from .cli import ReleaseError, console
from .github import RepoSlug, get_latest_release
from .repo import (
    fast_forward_current_branch,
    fetch_remote_branches,
    fetch_tag,
    get_commit,
    get_current_branch,
    is_ancestor,
)

# A release is cut from RELEASE_BRANCH, and ALIGNED_BRANCH is fast-forwarded
# to the release notes commit once the release is published.
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


def _require_head_on_release_branch() -> None:
    """Stop unless HEAD is on `dev`, so a release is never cut from, and its
    notes never committed on, another branch."""
    current_branch = get_current_branch()
    if current_branch != RELEASE_BRANCH:
        found = f"'{current_branch}'" if current_branch else "a detached commit"
        raise ReleaseError(
            f"releases are cut from '{RELEASE_BRANCH}' only, but HEAD is on {found}. "
            f"Run `git checkout {RELEASE_BRANCH}` and retry."
        )


def verify_release_branch_state() -> str:
    """Check the git state a release starts from, before the first stage does
    anything; return the `dev` commit, which the release is cut from.

    - HEAD is on `dev`, so the release is never cut from another branch,
    - `dev` matches `origin/dev`, so the release is the `dev` everyone sees,
    - `origin/main` is an ancestor of `dev`, so `main` can fast-forward to the
      release.
    """
    _require_head_on_release_branch()
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


def update_release_branch() -> str:
    """Fast-forward `dev`, checked out, to `origin/dev`, once `origin/dev` and
    `origin/main` are fetched; return the `dev` commit.

    `dev` takes merges while a release runs, and a publish reads `dev` when it
    starts and again before it pushes the notes, so it takes those merges in
    each time rather than stop on a `dev` behind `origin/dev`. A `dev` with
    commits `origin/dev` lacks is refused: the push of the notes would publish
    them.
    """
    _require_head_on_release_branch()
    console.print(f"Updating '{RELEASE_BRANCH}' from {GIT_REMOTE}...")
    fetch_remote_branches(GIT_REMOTE, (RELEASE_BRANCH, ALIGNED_BRANCH))

    dev_commit = get_commit("HEAD")
    remote_dev_commit = get_commit(f"{GIT_REMOTE}/{RELEASE_BRANCH}")
    if dev_commit == remote_dev_commit:
        return dev_commit
    if not is_ancestor(dev_commit, remote_dev_commit):
        raise ReleaseError(
            _describe_release_branch_drift(dev_commit, remote_dev_commit)
        )
    fast_forward_current_branch(remote_dev_commit)
    return remote_dev_commit


def verify_publish_branch_state(release_commit: str) -> str:
    """Check the git state a publish needs, before it writes anything; return
    the `dev` commit.

    `dev` may have taken merges since the release was cut from
    *release_commit*: the release stays that commit, and its notes are merged
    into `dev` whatever else `dev` holds. So `dev` is brought up to
    `origin/dev` (`update_release_branch`), and must still hold
    *release_commit*, so the notes merged into it, on top of that commit,
    bring back nothing `dev` dropped.

    Where `main` can go depends on the notes commit, which the publish looks
    up once this check passes.
    """
    dev_commit = update_release_branch()
    if not is_ancestor(release_commit, dev_commit):
        raise ReleaseError(
            f"'{RELEASE_BRANCH}' is at {dev_commit[:12]}, which does not hold "
            f"{release_commit[:12]}, the commit this release was built from, so "
            f"'{RELEASE_BRANCH}' was rewritten since the release started. This "
            f"publish stops before it writes anything; find out what rewrote "
            f"'{RELEASE_BRANCH}'."
        )
    return dev_commit


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
