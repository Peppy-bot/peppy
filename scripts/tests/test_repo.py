"""Tests for functions.repo module."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from functions.cli import ReleaseError
from functions.repo import (
    commit_paths,
    fetch_remote_branches,
    get_commit,
    get_commit_subjects,
    has_changes_in_paths,
    is_ancestor,
    is_branch_checked_out,
    push_branch,
    set_branch_ref,
    switch_branch,
    switch_to_new_branch,
)


def _mock_git(stdout: str, returncode: int = 0) -> MagicMock:
    mock = MagicMock()
    mock.returncode = returncode
    mock.stdout = stdout
    mock.stderr = "boom" if returncode else ""
    return mock


def test_get_commit_subjects_uses_range_when_base_given() -> None:
    with patch(
        "functions.repo.subprocess.run", return_value=_mock_git("a\nb\n")
    ) as run:
        subjects = get_commit_subjects("v0.1.0")
    assert subjects == ["a", "b"]
    cmd = run.call_args.args[0]
    assert cmd == ["git", "log", "--no-merges", "--format=%s", "v0.1.0..HEAD"]


def test_get_commit_subjects_full_history_when_base_none() -> None:
    with patch(
        "functions.repo.subprocess.run", return_value=_mock_git("only\n")
    ) as run:
        subjects = get_commit_subjects(None)
    assert subjects == ["only"]
    # No '..' range: the whole history up to HEAD.
    assert run.call_args.args[0][-1] == "HEAD"


def test_get_commit_subjects_skips_blank_lines() -> None:
    with patch(
        "functions.repo.subprocess.run", return_value=_mock_git("a\n\n  \nb\n")
    ):
        assert get_commit_subjects("v0.1.0") == ["a", "b"]


def test_get_commit_subjects_raises_on_git_failure() -> None:
    with patch(
        "functions.repo.subprocess.run", return_value=_mock_git("", returncode=128)
    ):
        with pytest.raises(ReleaseError, match="failed to read commit log"):
            get_commit_subjects("v9.9.9")


# --- resolving and comparing commits ---


def test_get_commit_resolves_revision_to_sha() -> None:
    with patch(
        "functions.repo.subprocess.run", return_value=_mock_git("abc123\n")
    ) as run:
        assert get_commit("origin/dev") == "abc123"
    assert run.call_args.args[0] == [
        "git",
        "rev-parse",
        "--verify",
        "origin/dev^{commit}",
    ]


def test_get_commit_raises_on_unknown_revision() -> None:
    with patch(
        "functions.repo.subprocess.run", return_value=_mock_git("", returncode=128)
    ):
        with pytest.raises(ReleaseError, match="could not resolve 'origin/nope'"):
            get_commit("origin/nope")


def test_is_ancestor_reads_exit_code() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("")) as run:
        assert is_ancestor("old", "new") is True
    assert run.call_args.args[0] == ["git", "merge-base", "--is-ancestor", "old", "new"]

    with patch("functions.repo.subprocess.run", return_value=_mock_git("", 1)):
        assert is_ancestor("side", "new") is False


def test_is_ancestor_raises_when_git_errors() -> None:
    # An unreadable revision exits 128; it must not be reported as "not an ancestor".
    with patch("functions.repo.subprocess.run", return_value=_mock_git("", 128)):
        with pytest.raises(ReleaseError, match="failed to compare 'old' with 'new'"):
            is_ancestor("old", "new")


# --- fetching, committing, and pushing ---


def test_fetch_remote_branches_uses_explicit_forced_refspecs() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("")) as run:
        fetch_remote_branches("origin", ("dev", "main"))
    assert run.call_args.args[0] == [
        "git",
        "fetch",
        "origin",
        "+refs/heads/dev:refs/remotes/origin/dev",
        "+refs/heads/main:refs/remotes/origin/main",
    ]


def test_fetch_remote_branches_raises_on_failure() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("", 1)):
        with pytest.raises(ReleaseError, match="failed to fetch dev, main"):
            fetch_remote_branches("origin", ("dev", "main"))


def test_has_changes_in_paths_reports_dirty_and_clean() -> None:
    with patch(
        "functions.repo.subprocess.run", return_value=_mock_git("?? docs/v1.html\n")
    ) as run:
        assert has_changes_in_paths([Path("docs/v1.html")]) is True
    assert run.call_args.args[0] == [
        "git",
        "status",
        "--porcelain",
        "--",
        "docs/v1.html",
    ]

    with patch("functions.repo.subprocess.run", return_value=_mock_git("")):
        assert has_changes_in_paths([Path("docs/v1.html")]) is False


def test_commit_paths_stages_then_commits_only_those_paths() -> None:
    with patch(
        "functions.repo.subprocess.run",
        side_effect=[_mock_git(""), _mock_git("")],
    ) as run:
        commit_paths([Path("docs/v1.html")], "docs: add release notes for v1")

    add_cmd, commit_cmd = (c.args[0] for c in run.call_args_list)
    assert add_cmd == ["git", "add", "--", "docs/v1.html"]
    # --only takes the commit content from the named paths, so unrelated staged
    # or dirty files are never swept into the release commit.
    assert commit_cmd == [
        "git",
        "commit",
        "--only",
        "-m",
        "docs: add release notes for v1",
        "--",
        "docs/v1.html",
    ]


def test_commit_paths_raises_when_commit_fails() -> None:
    with patch(
        "functions.repo.subprocess.run",
        side_effect=[_mock_git(""), _mock_git("", returncode=1)],
    ):
        with pytest.raises(ReleaseError, match="failed to commit docs/v1.html"):
            commit_paths([Path("docs/v1.html")], "docs: add release notes for v1")


def test_push_branch_pushes_a_refspec_without_force() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("")) as run:
        push_branch("origin", "dev", "main")
    assert run.call_args.args[0] == ["git", "push", "origin", "dev:refs/heads/main"]


def test_push_branch_raises_when_rejected() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("", 1)):
        with pytest.raises(ReleaseError, match="failed to push 'dev' to 'origin/main'"):
            push_branch("origin", "dev", "main")


# --- switching branches ---


def test_switch_to_new_branch_resets_an_existing_branch() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("")) as run:
        switch_to_new_branch("auto/docs-update-abc")
    # -C, not -c: a branch left over from an earlier attempt is reset, not a
    # collision.
    assert run.call_args.args[0] == ["git", "switch", "-C", "auto/docs-update-abc"]


def test_switch_to_new_branch_raises_when_checked_out_elsewhere() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("", 1)):
        with pytest.raises(ReleaseError, match="failed to switch to a new branch"):
            switch_to_new_branch("auto/docs-update-abc")


def test_switch_branch_checks_out_an_existing_branch() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("")) as run:
        switch_branch("dev")
    assert run.call_args.args[0] == ["git", "switch", "dev"]


def test_switch_branch_raises_on_failure() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("", 1)):
        with pytest.raises(ReleaseError, match="failed to switch back to 'dev'"):
            switch_branch("dev")


# --- local branch refs ---


_WORKTREE_LIST = (
    "worktree /repo\n"
    "HEAD 1111111111111111111111111111111111111111\n"
    "branch refs/heads/dev\n"
    "\n"
    "worktree /repo-main\n"
    "HEAD 2222222222222222222222222222222222222222\n"
    "branch refs/heads/main\n"
)


def test_is_branch_checked_out_finds_branch_in_another_worktree() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git(_WORKTREE_LIST)):
        assert is_branch_checked_out("main") is True
        assert is_branch_checked_out("release") is False


def test_is_branch_checked_out_ignores_prefix_matches() -> None:
    # 'main' must not match a worktree sitting on 'maintenance'.
    listing = "worktree /repo\nHEAD 1111\nbranch refs/heads/maintenance\n"
    with patch("functions.repo.subprocess.run", return_value=_mock_git(listing)):
        assert is_branch_checked_out("main") is False


def test_set_branch_ref_updates_ref_without_checkout() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("")) as run:
        set_branch_ref("main", "abc123")
    assert run.call_args.args[0] == ["git", "update-ref", "refs/heads/main", "abc123"]


def test_set_branch_ref_raises_on_failure() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("", 1)):
        with pytest.raises(ReleaseError, match="failed to point 'main' at abc123"):
            set_branch_ref("main", "abc123")
