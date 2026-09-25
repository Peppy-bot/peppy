"""Tests for functions.repo module."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from functions.cli import ReleaseError
from functions.repo import (
    commit_paths,
    detached_worktree,
    fast_forward_current_branch,
    fetch_remote_branches,
    fetch_tag,
    find_commit,
    get_changed_paths,
    get_commit,
    get_commit_subjects,
    get_commits_changing,
    get_parents,
    has_changes_in_paths,
    is_ancestor,
    merge_into_current_branch,
    push_branch,
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


def test_find_commit_resolves_an_abbreviated_sha() -> None:
    with patch(
        "functions.repo.subprocess.run", return_value=_mock_git("abc123def456\n")
    ) as run:
        assert find_commit("abc123") == "abc123def456"
    assert run.call_args.args[0] == [
        "git",
        "rev-parse",
        "--verify",
        "--quiet",
        "abc123^{commit}",
    ]


@pytest.mark.parametrize("returncode", [1, 128])
def test_find_commit_is_none_for_a_revision_naming_no_commit(returncode: int) -> None:
    # Unknown to this clone, or an abbreviation it cannot tell apart.
    with patch(
        "functions.repo.subprocess.run",
        return_value=_mock_git("", returncode=returncode),
    ):
        assert find_commit("abc123") is None


def test_get_parents_lists_the_parents_in_order() -> None:
    with patch(
        "functions.repo.subprocess.run", return_value=_mock_git("ccc aaa bbb\n")
    ) as run:
        assert get_parents("ccc") == ("aaa", "bbb")
    assert run.call_args.args[0] == [
        "git",
        "rev-list",
        "--parents",
        "--max-count=1",
        "ccc",
    ]


def test_get_parents_of_a_root_commit_is_empty() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("aaa\n")):
        assert get_parents("aaa") == ()


def test_get_parents_raises_on_an_unknown_commit() -> None:
    with patch(
        "functions.repo.subprocess.run", return_value=_mock_git("", returncode=128)
    ):
        with pytest.raises(ReleaseError, match="failed to read the parents of 'nope'"):
            get_parents("nope")


def test_get_changed_paths_lists_every_path_without_renames() -> None:
    # NUL-separated, so a path is read back whole whatever characters it holds.
    with patch(
        "functions.repo.subprocess.run",
        return_value=_mock_git("docs/a b.html\0old.rs\0new.rs\0"),
    ) as run:
        assert get_changed_paths("aaa", "bbb") == ("docs/a b.html", "old.rs", "new.rs")
    assert run.call_args.args[0] == [
        "git",
        "diff",
        "--name-only",
        "--no-renames",
        "-z",
        "aaa",
        "bbb",
    ]


def test_get_changed_paths_raises_when_git_errors() -> None:
    with patch(
        "functions.repo.subprocess.run", return_value=_mock_git("", returncode=128)
    ):
        with pytest.raises(ReleaseError, match="failed to list the paths changed"):
            get_changed_paths("aaa", "bbb")


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


def test_fetch_tag_fetches_only_that_tag() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("")) as run:
        fetch_tag("origin", "v0.2.0")
    assert run.call_args.args[0] == [
        "git",
        "fetch",
        "--no-tags",
        "origin",
        "refs/tags/v0.2.0:refs/tags/v0.2.0",
    ]


def test_fetch_tag_raises_on_failure() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("", 1)):
        with pytest.raises(ReleaseError, match="failed to fetch tag 'v0.2.0'"):
            fetch_tag("origin", "v0.2.0")


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
        side_effect=[_mock_git(""), _mock_git(""), _mock_git("abc123\n")],
    ) as run:
        commit = commit_paths([Path("docs/v1.html")], "docs: add release notes for v1")

    assert commit == "abc123"
    add_cmd, commit_cmd, head_cmd = (c.args[0] for c in run.call_args_list)
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
    assert head_cmd == ["git", "rev-parse", "HEAD"]
    # Without a checkout named, every command runs in the current directory.
    assert {c.kwargs["cwd"] for c in run.call_args_list} == {None}


def test_commit_paths_commits_in_the_checkout_named() -> None:
    tree = Path("/tmp/worktree")
    with patch(
        "functions.repo.subprocess.run",
        side_effect=[_mock_git(""), _mock_git(""), _mock_git("abc123\n")],
    ) as run:
        commit_paths([tree / "docs/v1.html"], "docs: v1", cwd=tree)

    assert {c.kwargs["cwd"] for c in run.call_args_list} == {tree}


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


def test_push_branch_pushes_a_commit() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("")) as run:
        push_branch("origin", "a" * 40, "main")
    assert run.call_args.args[0] == [
        "git",
        "push",
        "origin",
        f"{'a' * 40}:refs/heads/main",
    ]


def test_push_branch_raises_when_rejected() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("", 1)):
        with pytest.raises(ReleaseError, match="failed to push 'dev' to 'origin/main'"):
            push_branch("origin", "dev", "main")


# --- commits that change a path ---


def test_get_commits_changing_searches_every_side_of_every_merge() -> None:
    with patch(
        "functions.repo.subprocess.run", return_value=_mock_git("bbb\naaa\n")
    ) as run:
        commits = get_commits_changing("base", "dev", Path("docs/v1.html"))

    assert commits == ("bbb", "aaa")
    assert run.call_args.args[0] == [
        "git",
        "rev-list",
        "--full-history",
        "--no-merges",
        "base..dev",
        "--",
        "docs/v1.html",
    ]


def test_get_commits_changing_is_empty_when_no_commit_changes_the_path() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("")):
        assert get_commits_changing("base", "dev", Path("docs/v1.html")) == ()


def test_get_commits_changing_raises_when_git_errors() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("", 128)):
        with pytest.raises(ReleaseError, match="that change 'docs/v1.html': boom"):
            get_commits_changing("base", "dev", Path("docs/v1.html"))


# --- merges ---


def test_fast_forward_current_branch_refuses_anything_else() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("")) as run:
        fast_forward_current_branch("origin/dev")
    assert run.call_args.args[0] == [
        "git",
        "merge",
        "--ff-only",
        "--quiet",
        "origin/dev",
    ]


def test_fast_forward_current_branch_raises_on_failure() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("", 128)):
        with pytest.raises(ReleaseError, match="failed to fast-forward to 'origin/dev'"):
            fast_forward_current_branch("origin/dev")


def test_merge_into_current_branch_fast_forwards_when_it_can() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("")) as run:
        merge_into_current_branch("abc123", "Merge the notes")
    # --ff overrides a merge.ff of the host's configuration, and --no-edit
    # keeps the merge from asking for its message.
    assert run.call_args.args[0] == [
        "git",
        "merge",
        "--ff",
        "--no-edit",
        "-m",
        "Merge the notes",
        "abc123",
    ]


def test_merge_into_current_branch_raises_on_a_conflict() -> None:
    conflict = _mock_git("CONFLICT (add/add): Merge conflict in docs/v1.html", 1)
    conflict.stderr = ""
    with patch("functions.repo.subprocess.run", return_value=conflict):
        with pytest.raises(ReleaseError, match="CONFLICT"):
            merge_into_current_branch("abc123", "Merge the notes")


# --- worktrees ---


def test_detached_worktree_is_removed_when_its_body_fails() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("")) as run:
        with pytest.raises(ReleaseError, match="the body failed"):
            with detached_worktree("abc123") as tree:
                raise ReleaseError("the body failed")

    add_cmd, remove_cmd = (c.args[0] for c in run.call_args_list)
    assert add_cmd == ["git", "worktree", "add", "--quiet", "--detach", str(tree), "abc123"]
    assert remove_cmd == ["git", "worktree", "remove", "--force", str(tree)]
    # The temporary directory that held it is gone too.
    assert not tree.parent.exists()


def test_detached_worktree_raises_when_it_cannot_be_added() -> None:
    with patch("functions.repo.subprocess.run", return_value=_mock_git("", 128)) as run:
        with pytest.raises(ReleaseError, match="failed to add a worktree at 'abc123'"):
            with detached_worktree("abc123"):
                pass

    # Nothing was added, so nothing is removed.
    assert run.call_count == 1


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
