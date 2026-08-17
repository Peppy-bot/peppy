"""Git repository operations: repo root, branch state, commits, and pushes."""

from __future__ import annotations

import subprocess
from collections.abc import Sequence
from pathlib import Path

from .cli import ReleaseError


def get_repo_root() -> Path:
    """Return the git repository root directory.

    Raises ReleaseError if not inside a git repository.
    """
    result = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError("must be run inside a git repository")
    return Path(result.stdout.strip())


def get_current_branch() -> str | None:
    """Return the current branch name, or None if in detached HEAD state."""
    result = subprocess.run(
        ["git", "rev-parse", "--abbrev-ref", "HEAD"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        return None
    branch = result.stdout.strip()
    if branch == "HEAD":
        return None
    return branch


def has_uncommitted_changes() -> bool:
    """Check for uncommitted changes (staged or unstaged).

    Returns True if there are any uncommitted changes in the working tree.
    """
    staged = subprocess.run(
        ["git", "diff", "--cached", "--quiet"],
        capture_output=True,
    )
    unstaged = subprocess.run(
        ["git", "diff", "--quiet"],
        capture_output=True,
    )
    return staged.returncode != 0 or unstaged.returncode != 0


def get_commit(rev: str) -> str:
    """Resolve a revision (branch, tag, HEAD, SHA) to its commit SHA.

    Raises ReleaseError if the revision does not resolve to a commit locally.
    """
    result = subprocess.run(
        ["git", "rev-parse", "--verify", f"{rev}^{{commit}}"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(
            f"could not resolve '{rev}' to a commit: {result.stderr.strip()}"
        )
    return result.stdout.strip()


def is_ancestor(ancestor: str, descendant: str) -> bool:
    """Return True if *ancestor* is reachable from *descendant*.

    Raises ReleaseError if either revision cannot be read, so an unresolvable
    ref is never mistaken for "not an ancestor".
    """
    result = subprocess.run(
        ["git", "merge-base", "--is-ancestor", ancestor, descendant],
        capture_output=True,
        text=True,
    )
    if result.returncode in (0, 1):
        return result.returncode == 0
    raise ReleaseError(
        f"failed to compare '{ancestor}' with '{descendant}': {result.stderr.strip()}"
    )


def get_commit_subjects(base: str | None, head: str = "HEAD") -> list[str]:
    """Return the commit subjects between base and head (newest first).

    When base is None (no prior release), returns the full history up to head.
    Merge commits are excluded, so the list reflects the actual changes rather
    than pull-request merge noise.

    Raises ReleaseError if the revision range cannot be read (for example, when
    the base tag has not been fetched locally).
    """
    rev_range = f"{base}..{head}" if base else head
    result = subprocess.run(
        ["git", "log", "--no-merges", "--format=%s", rev_range],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(
            f"failed to read commit log for '{rev_range}': "
            f"{result.stderr.strip()} (try 'git fetch --tags')"
        )
    return [line.strip() for line in result.stdout.splitlines() if line.strip()]


def fetch_remote_branches(remote: str, branches: Sequence[str]) -> None:
    """Refresh the remote-tracking refs for *branches* from *remote*.

    Uses explicit refspecs so ``{remote}/{branch}`` is guaranteed to reflect the
    remote after this call, and forces the update so a rewound remote branch is
    reported as it actually is.

    Raises ReleaseError if the fetch fails or a branch is missing on the remote.
    """
    refspecs = [f"+refs/heads/{b}:refs/remotes/{remote}/{b}" for b in branches]
    result = subprocess.run(
        ["git", "fetch", remote, *refspecs],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(
            f"failed to fetch {', '.join(branches)} from '{remote}': "
            f"{result.stderr.strip()}"
        )


def has_changes_in_paths(paths: Sequence[Path]) -> bool:
    """Return True if any of *paths* is modified, staged, or untracked.

    Raises ReleaseError if the status cannot be read.
    """
    result = subprocess.run(
        ["git", "status", "--porcelain", "--", *(str(p) for p in paths)],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(f"failed to read git status: {result.stderr.strip()}")
    return bool(result.stdout.strip())


def commit_paths(paths: Sequence[Path], message: str) -> None:
    """Commit exactly *paths*, leaving every other change in the tree alone.

    ``git commit --only`` takes its content from the named paths rather than the
    index, so an unrelated dirty file or staged change is never swept into the
    release commit.

    Raises ReleaseError if staging or committing fails.
    """
    path_args = [str(p) for p in paths]
    staged = subprocess.run(
        ["git", "add", "--", *path_args],
        capture_output=True,
        text=True,
    )
    if staged.returncode != 0:
        raise ReleaseError(
            f"failed to stage {', '.join(path_args)}: {staged.stderr.strip()}"
        )

    committed = subprocess.run(
        ["git", "commit", "--only", "-m", message, "--", *path_args],
        capture_output=True,
        text=True,
    )
    if committed.returncode != 0:
        raise ReleaseError(
            f"failed to commit {', '.join(path_args)}: "
            f"{committed.stderr.strip() or committed.stdout.strip()}"
        )


def push_branch(remote: str, local_branch: str, remote_branch: str) -> None:
    """Push *local_branch* to ``{remote}/{remote_branch}``.

    The push is a plain (non-forced) one, so it is rejected unless it is a
    fast-forward. Raises ReleaseError if the push fails.
    """
    result = subprocess.run(
        ["git", "push", remote, f"{local_branch}:refs/heads/{remote_branch}"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(
            f"failed to push '{local_branch}' to '{remote}/{remote_branch}': "
            f"{result.stderr.strip()}"
        )


def force_push_branch(remote: str, local_branch: str, remote_branch: str) -> None:
    """Force-push *local_branch* to ``{remote}/{remote_branch}``.

    Kept separate from `push_branch` so that overwriting remote history is
    always visible at the call site: the only caller is the throwaway docs-sync
    branch, whose content is regenerated from scratch on every release attempt
    and must replace whatever a previous attempt left behind.

    Raises ReleaseError if the push fails.
    """
    result = subprocess.run(
        ["git", "push", "--force", remote, f"{local_branch}:refs/heads/{remote_branch}"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(
            f"failed to force-push '{local_branch}' to '{remote}/{remote_branch}': "
            f"{result.stderr.strip()}"
        )


def switch_to_new_branch(branch: str) -> None:
    """Create or reset *branch* at HEAD and check it out, keeping the tree.

    ``git switch -C`` resets an existing branch instead of refusing, so a branch
    left over from an earlier attempt is reused rather than colliding. The
    working tree is carried over, which is the point: the caller has just edited
    files and wants them committed here rather than on the current branch.

    Raises ReleaseError if the branch cannot be checked out (for example when
    another worktree already has it).
    """
    result = subprocess.run(
        ["git", "switch", "-C", branch],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(
            f"failed to switch to a new branch '{branch}': {result.stderr.strip()}"
        )


def switch_branch(branch: str) -> None:
    """Check out an existing *branch*, carrying uncommitted changes over.

    Raises ReleaseError if the branch cannot be checked out.
    """
    result = subprocess.run(
        ["git", "switch", branch],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(
            f"failed to switch back to '{branch}': {result.stderr.strip()}"
        )


def is_branch_checked_out(branch: str) -> bool:
    """Return True if *branch* is the checked-out branch of any worktree.

    Raises ReleaseError if the worktree list cannot be read.
    """
    result = subprocess.run(
        ["git", "worktree", "list", "--porcelain"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(f"failed to list git worktrees: {result.stderr.strip()}")
    return f"branch refs/heads/{branch}" in result.stdout.splitlines()


def set_branch_ref(branch: str, commit: str) -> None:
    """Point the local *branch* at *commit* without checking it out.

    Only safe when *branch* is checked out nowhere (see `is_branch_checked_out`),
    because moving the ref under a worktree that has it checked out leaves that
    worktree's index disagreeing with HEAD.

    Raises ReleaseError if the ref cannot be updated.
    """
    result = subprocess.run(
        ["git", "update-ref", f"refs/heads/{branch}", commit],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(
            f"failed to point '{branch}' at {commit}: {result.stderr.strip()}"
        )
