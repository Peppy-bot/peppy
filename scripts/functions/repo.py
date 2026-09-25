"""Git repository operations: repo root, branch state, commits, and pushes."""

from __future__ import annotations

import subprocess
import tempfile
from collections.abc import Iterator, Sequence
from contextlib import contextmanager
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


def find_commit(rev: str) -> str | None:
    """Resolve *rev* to its commit SHA, or None when it names no commit locally.

    For a revision that may be absent: an abbreviated SHA read off a branch
    name, whose commit this clone never fetched or no longer tells apart from
    another. `get_commit` is for a revision that has to exist.
    """
    result = subprocess.run(
        ["git", "rev-parse", "--verify", "--quiet", f"{rev}^{{commit}}"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        return None
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


def get_parents(commit: str) -> tuple[str, ...]:
    """Return the parents of *commit*, in order: none for a root commit, two or
    more for a merge.

    Raises ReleaseError if *commit* cannot be read.
    """
    result = subprocess.run(
        ["git", "rev-list", "--parents", "--max-count=1", commit],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(
            f"failed to read the parents of '{commit}': {result.stderr.strip()}"
        )
    # One line: the commit itself, then its parents.
    return tuple(result.stdout.split()[1:])


def get_changed_paths(base: str, head: str) -> tuple[str, ...]:
    """Return every path that differs between *base* and *head*, relative to
    the repository root.

    Renames are listed as the path removed and the path added, so a moved file
    never passes for a change to one path alone.

    Raises ReleaseError if either revision cannot be read.
    """
    result = subprocess.run(
        ["git", "diff", "--name-only", "--no-renames", "-z", base, head],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(
            f"failed to list the paths changed between '{base}' and '{head}': "
            f"{result.stderr.strip()}"
        )
    return tuple(path for path in result.stdout.split("\0") if path)


def get_commits_changing(base: str, head: str, path: Path) -> tuple[str, ...]:
    """Return the commits reachable from *head* and not from *base* that change
    *path*, newest first.

    Every side of every merge is searched (`--full-history`), and merge commits
    are left out, so each commit listed is one that made its change itself.

    Raises ReleaseError if either revision cannot be read.
    """
    result = subprocess.run(
        [
            "git",
            "rev-list",
            "--full-history",
            "--no-merges",
            f"{base}..{head}",
            "--",
            path.as_posix(),
        ],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(
            f"failed to list the commits between '{base}' and '{head}' that "
            f"change '{path}': {result.stderr.strip()}"
        )
    return tuple(result.stdout.split())


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


def fetch_tag(remote: str, tag: str) -> None:
    """Fetch *tag* from *remote* into the local tags.

    A clone holds only the tags that existed when it was made, so a tag created
    on the remote since then has to be fetched before it can be read.

    Raises ReleaseError if the fetch fails, the tag is missing on the remote, or
    a local tag of the same name points elsewhere.
    """
    result = subprocess.run(
        ["git", "fetch", "--no-tags", remote, f"refs/tags/{tag}:refs/tags/{tag}"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(
            f"failed to fetch tag '{tag}' from '{remote}': {result.stderr.strip()}"
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


def commit_paths(
    paths: Sequence[Path], message: str, *, cwd: Path | None = None
) -> str:
    """Commit exactly *paths* in the checkout at *cwd* (the current directory
    when None), leaving every other change in the tree alone; return the new
    commit.

    ``git commit --only`` takes its content from the named paths rather than the
    index, so an unrelated dirty file or staged change is never swept into the
    release commit.

    Raises ReleaseError if staging or committing fails.
    """
    path_args = [str(p) for p in paths]
    staged = subprocess.run(
        ["git", "add", "--", *path_args],
        cwd=cwd,
        capture_output=True,
        text=True,
    )
    if staged.returncode != 0:
        raise ReleaseError(
            f"failed to stage {', '.join(path_args)}: {staged.stderr.strip()}"
        )

    committed = subprocess.run(
        ["git", "commit", "--only", "-m", message, "--", *path_args],
        cwd=cwd,
        capture_output=True,
        text=True,
    )
    if committed.returncode != 0:
        raise ReleaseError(
            f"failed to commit {', '.join(path_args)}: "
            f"{committed.stderr.strip() or committed.stdout.strip()}"
        )

    head = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=cwd,
        capture_output=True,
        text=True,
    )
    if head.returncode != 0:
        raise ReleaseError(
            f"failed to read the commit of {', '.join(path_args)}: "
            f"{head.stderr.strip()}"
        )
    return head.stdout.strip()


def fast_forward_current_branch(commit: str) -> None:
    """Fast-forward the branch checked out to *commit*, a descendant of it.

    Raises ReleaseError if the fast-forward fails.
    """
    result = subprocess.run(
        ["git", "merge", "--ff-only", "--quiet", commit],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(
            f"failed to fast-forward to '{commit}': "
            f"{result.stderr.strip() or result.stdout.strip()}"
        )


def merge_into_current_branch(commit: str, message: str) -> None:
    """Merge *commit* into the branch checked out: a fast-forward when the
    branch is an ancestor of *commit*, else a merge commit with *message*.

    Raises ReleaseError if the merge fails, a conflict included.
    """
    result = subprocess.run(
        ["git", "merge", "--ff", "--no-edit", "-m", message, commit],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(
            f"failed to merge '{commit}': "
            f"{result.stderr.strip() or result.stdout.strip()}"
        )


@contextmanager
def detached_worktree(commit: str) -> Iterator[Path]:
    """Yield a temporary worktree of this repository, detached at *commit*.

    A commit is made on top of *commit* there while the checkout stays on its
    branch. The worktree is removed on exit, whatever happens inside.

    Raises ReleaseError if the worktree cannot be added.
    """
    with tempfile.TemporaryDirectory(prefix="peppy-worktree-") as parent:
        tree = Path(parent) / "tree"
        added = subprocess.run(
            ["git", "worktree", "add", "--quiet", "--detach", str(tree), commit],
            capture_output=True,
            text=True,
        )
        if added.returncode != 0:
            raise ReleaseError(
                f"failed to add a worktree at '{commit}': {added.stderr.strip()}"
            )
        try:
            yield tree
        finally:
            subprocess.run(
                ["git", "worktree", "remove", "--force", str(tree)],
                capture_output=True,
            )


def push_branch(remote: str, source: str, remote_branch: str) -> None:
    """Push *source*, a local branch or a commit, to ``{remote}/{remote_branch}``.

    The push is a plain (non-forced) one, so it is rejected unless it is a
    fast-forward. Raises ReleaseError if the push fails.
    """
    result = subprocess.run(
        ["git", "push", remote, f"{source}:refs/heads/{remote_branch}"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(
            f"failed to push '{source}' to '{remote}/{remote_branch}': "
            f"{result.stderr.strip()}"
        )


def switch_to_new_branch(branch: str) -> None:
    """Create or reset *branch* at HEAD and check it out, keeping the tree.

    ``git switch -C`` resets an existing branch instead of refusing, so a local
    branch left over from an earlier attempt is reused rather than colliding.
    Only the local ref moves; nothing published is rewritten, and the push that
    follows is a plain one that fails if the remote has diverged. The working
    tree is carried over, which is the point: the caller has just edited files
    and wants them committed here rather than on the current branch.

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
