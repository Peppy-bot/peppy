"""Git repository operations: repo root, branch checking, uncommitted changes, tag verification."""

from __future__ import annotations

import subprocess
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


def get_tag_commit(tag: str) -> str:
    """Resolve a tag to its commit SHA.

    Raises ReleaseError if the tag doesn't exist locally.
    """
    result = subprocess.run(
        ["git", "rev-parse", f"{tag}^{{commit}}"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(f"tag '{tag}' not found locally (run 'git fetch --tags')")
    return result.stdout.strip()


def get_head_commit() -> str:
    """Return the SHA of the current HEAD commit."""
    result = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError("failed to get HEAD commit")
    return result.stdout.strip()


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


def checkout(ref: str) -> None:
    """Checkout a git ref (tag, branch, or commit).

    Raises ReleaseError if the checkout fails.
    """
    result = subprocess.run(
        ["git", "checkout", ref],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(f"failed to checkout '{ref}': {result.stderr.strip()}")
