"""Tests for functions.release_docs_gate."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from functions.cli import ReleaseError
from functions.docs import CheckResult, RequiredChange, UpdateOutcome, UpdateResult
from functions.github import RepoSlug
from functions.release_docs_gate import (
    JudgedCommit,
    _docs_check_base,
    _find_open_docs_sync_pr,
    _judged_commit_of,
    _last_judged_commit,
    _last_shipped_commit,
    _open_docs_pr,
    _push_docs_sync_branch,
    verify_docs_gate,
)

DEV_COMMIT = "1111111111111111111111111111111111111111"


@pytest.fixture(autouse=True)
def _no_prompts() -> object:
    """The gate never asks anything: every prompt fails the test."""
    with patch(
        "functions.cli.Prompt.ask", side_effect=AssertionError("prompted")
    ), patch("functions.cli.Confirm.ask", side_effect=AssertionError("prompted")):
        yield


def _unwrapped(console_output: str) -> str:
    """Console output with rich's line wrapping undone, for phrase assertions."""
    return " ".join(console_output.split())


def _ancestry_resolver(ancestry: dict[tuple[str, str], bool]):
    """Return an is_ancestor stub answering the given (ancestor, descendant) pairs."""
    return lambda ancestor, descendant: ancestry[(ancestor, descendant)]


# --- the last shipped commit ---

LATEST_TAG = "v0.29.0"


@patch("functions.release_docs_gate.is_ancestor")
@patch("functions.release_docs_gate.latest_release_tag", return_value=None)
def test_last_shipped_commit_is_origin_main_without_a_published_release(
    mock_tag: MagicMock, mock_is_ancestor: MagicMock
) -> None:
    assert _last_shipped_commit(MagicMock(), RepoSlug("o", "r"), DEV_COMMIT) == "origin/main"

    mock_is_ancestor.assert_not_called()


@patch("functions.release_docs_gate.is_ancestor")
@patch("functions.release_docs_gate.latest_release_tag", return_value=LATEST_TAG)
def test_last_shipped_commit_is_origin_main_once_aligned_to_the_latest_release(
    mock_tag: MagicMock, mock_is_ancestor: MagicMock
) -> None:
    # main sits one notes commit past the tag, as a finished release leaves it.
    mock_is_ancestor.side_effect = _ancestry_resolver(
        {(LATEST_TAG, "origin/main"): True}
    )

    assert _last_shipped_commit(MagicMock(), RepoSlug("o", "r"), DEV_COMMIT) == "origin/main"


@patch("functions.release_docs_gate.is_ancestor")
@patch("functions.release_docs_gate.latest_release_tag", return_value=LATEST_TAG)
def test_last_shipped_commit_is_the_latest_release_tag_when_main_lags_it(
    mock_tag: MagicMock,
    mock_is_ancestor: MagicMock,
    capfd: pytest.CaptureFixture[str],
) -> None:
    # A release published but never aligned: the tag is past main, on the way
    # to the commit being released.
    mock_is_ancestor.side_effect = _ancestry_resolver(
        {
            (LATEST_TAG, "origin/main"): False,
            ("origin/main", LATEST_TAG): True,
            (LATEST_TAG, DEV_COMMIT): True,
        }
    )

    assert _last_shipped_commit(MagicMock(), RepoSlug("o", "r"), DEV_COMMIT) == LATEST_TAG

    err = _unwrapped(capfd.readouterr().err)
    assert f"origin/main is behind the latest release {LATEST_TAG}" in err


@pytest.mark.parametrize(
    "ancestry",
    [
        # A tag cut from main itself, off the dev line.
        {
            (LATEST_TAG, "origin/main"): False,
            ("origin/main", LATEST_TAG): True,
            (LATEST_TAG, DEV_COMMIT): False,
        },
        # A tag on neither branch.
        {
            (LATEST_TAG, "origin/main"): False,
            ("origin/main", LATEST_TAG): False,
        },
    ],
)
@patch("functions.release_docs_gate.is_ancestor")
@patch("functions.release_docs_gate.latest_release_tag", return_value=LATEST_TAG)
def test_last_shipped_commit_stays_origin_main_when_the_tag_is_off_the_release_line(
    mock_tag: MagicMock,
    mock_is_ancestor: MagicMock,
    ancestry: dict[tuple[str, str], bool],
) -> None:
    mock_is_ancestor.side_effect = _ancestry_resolver(ancestry)

    assert _last_shipped_commit(MagicMock(), RepoSlug("o", "r"), DEV_COMMIT) == "origin/main"


# --- the commit a merged docs pull request settled ---

JUDGED_COMMIT = "4444444444444444444444444444444444444444"
JUDGED_MERGE = "5555555555555555555555555555555555555555"
EARLIER_JUDGED_COMMIT = "6666666666666666666666666666666666666666"
EARLIER_JUDGED_MERGE = "7777777777777777777777777777777777777777"
JUDGED_PR_URL = "https://github.com/o/r/pull/12"


def _docs_sync_pull(
    judged: str = JUDGED_COMMIT,
    merge: str | None = JUDGED_MERGE,
    *,
    merged: bool = True,
    branch_prefix: str = "auto/docs-update-",
    url: str = JUDGED_PR_URL,
) -> dict:
    """A closed pull request as the GitHub API lists it."""
    return {
        "html_url": url,
        "merged_at": "2026-09-19T14:30:00Z" if merged else None,
        "merge_commit_sha": merge,
        "head": {"ref": f"{branch_prefix}{judged[:12]}"},
    }


def _known_commits(*commits: str):
    """A find_commit stub resolving full or abbreviated SHAs of *commits*."""

    def _find(rev: str) -> str | None:
        return next((commit for commit in commits if commit.startswith(rev)), None)

    return _find


_ON_THE_RELEASE_LINE = {
    ("origin/main", JUDGED_COMMIT): True,
    (JUDGED_COMMIT, DEV_COMMIT): True,
    (JUDGED_MERGE, DEV_COMMIT): True,
}


@patch("functions.release_docs_gate.is_ancestor")
@patch("functions.release_docs_gate.find_commit", _known_commits(JUDGED_COMMIT, JUDGED_MERGE))
def test_judged_commit_of_a_merged_docs_sync_pull_request(
    mock_is_ancestor: MagicMock,
) -> None:
    mock_is_ancestor.side_effect = _ancestry_resolver(_ON_THE_RELEASE_LINE)

    judged = _judged_commit_of(_docs_sync_pull(), "origin/main", DEV_COMMIT)

    # The branch names the commit by its first characters: the full SHA is read back.
    assert judged == JudgedCommit(commit=JUDGED_COMMIT, pr_url=JUDGED_PR_URL)


@pytest.mark.parametrize(
    "pull",
    [
        # Closed without being merged: its gaps were never closed.
        _docs_sync_pull(merged=False, merge=None),
        # A polish pull request never closes a blocking gap.
        _docs_sync_pull(branch_prefix="auto/docs-polish-"),
        # Any other pull request into dev.
        _docs_sync_pull(branch_prefix="fix-the-router-"),
        # Not the shape the API answers with.
        "not a pull request",
        {"merged_at": "2026-09-19T14:30:00Z", "head": None},
    ],
)
@patch("functions.release_docs_gate.is_ancestor")
@patch("functions.release_docs_gate.find_commit", _known_commits(JUDGED_COMMIT, JUDGED_MERGE))
def test_judged_commit_of_is_none_for_a_pull_request_that_settles_nothing(
    mock_is_ancestor: MagicMock, pull: object
) -> None:
    assert _judged_commit_of(pull, "origin/main", DEV_COMMIT) is None

    mock_is_ancestor.assert_not_called()


@pytest.mark.parametrize(
    "ancestry",
    [
        # Judged before the last release shipped: that release's check covers it.
        {("origin/main", JUDGED_COMMIT): False},
        # Judged on a line the release does not descend from.
        {("origin/main", JUDGED_COMMIT): True, (JUDGED_COMMIT, DEV_COMMIT): False},
        # Merged, but past the commit being released: its docs are not in it.
        {
            ("origin/main", JUDGED_COMMIT): True,
            (JUDGED_COMMIT, DEV_COMMIT): True,
            (JUDGED_MERGE, DEV_COMMIT): False,
        },
    ],
)
@patch("functions.release_docs_gate.is_ancestor")
@patch("functions.release_docs_gate.find_commit", _known_commits(JUDGED_COMMIT, JUDGED_MERGE))
def test_judged_commit_of_is_none_off_the_way_to_the_release(
    mock_is_ancestor: MagicMock, ancestry: dict[tuple[str, str], bool]
) -> None:
    mock_is_ancestor.side_effect = _ancestry_resolver(ancestry)

    assert _judged_commit_of(_docs_sync_pull(), "origin/main", DEV_COMMIT) is None


@pytest.mark.parametrize(
    "known",
    [
        # The branch names a commit this clone does not hold.
        (JUDGED_MERGE,),
        # So does the merge.
        (JUDGED_COMMIT,),
    ],
)
@patch("functions.release_docs_gate.is_ancestor")
def test_judged_commit_of_is_none_when_a_commit_is_unknown_locally(
    mock_is_ancestor: MagicMock, known: tuple[str, ...]
) -> None:
    with patch("functions.release_docs_gate.find_commit", _known_commits(*known)):
        assert _judged_commit_of(_docs_sync_pull(), "origin/main", DEV_COMMIT) is None

    mock_is_ancestor.assert_not_called()


@patch("functions.release_docs_gate.is_ancestor")
@patch(
    "functions.release_docs_gate.find_commit",
    _known_commits(
        JUDGED_COMMIT, JUDGED_MERGE, EARLIER_JUDGED_COMMIT, EARLIER_JUDGED_MERGE
    ),
)
@patch("functions.release_docs_gate.github_api")
def test_last_judged_commit_is_the_latest_one_on_the_way_to_the_release(
    mock_api: MagicMock, mock_is_ancestor: MagicMock
) -> None:
    earlier_url = "https://github.com/o/r/pull/11"
    # The API lists by last update, which is not the order of the commits.
    mock_api.return_value = [
        _docs_sync_pull(EARLIER_JUDGED_COMMIT, EARLIER_JUDGED_MERGE, url=earlier_url),
        _docs_sync_pull(),
        _docs_sync_pull(branch_prefix="fix-the-router-"),
    ]
    mock_is_ancestor.side_effect = _ancestry_resolver(
        {
            **_ON_THE_RELEASE_LINE,
            ("origin/main", EARLIER_JUDGED_COMMIT): True,
            (EARLIER_JUDGED_COMMIT, DEV_COMMIT): True,
            (EARLIER_JUDGED_MERGE, DEV_COMMIT): True,
            (EARLIER_JUDGED_COMMIT, JUDGED_COMMIT): True,
        }
    )

    judged = _last_judged_commit(
        MagicMock(), RepoSlug("test-owner", "test-repo"), "origin/main", DEV_COMMIT
    )

    assert judged == JudgedCommit(commit=JUDGED_COMMIT, pr_url=JUDGED_PR_URL)
    query = mock_api.call_args.args[2]
    assert "/repos/test-owner/test-repo/pulls?" in query
    assert "base=dev" in query
    assert "state=closed" in query
    assert "sort=updated&direction=desc" in query
    assert "per_page=100" in query


@patch("functions.release_docs_gate.github_api", return_value=[])
def test_last_judged_commit_is_none_without_a_merged_docs_pull_request(
    mock_api: MagicMock,
) -> None:
    assert (
        _last_judged_commit(MagicMock(), RepoSlug("o", "r"), "origin/main", DEV_COMMIT)
        is None
    )


@patch("functions.release_docs_gate._last_judged_commit", return_value=None)
@patch("functions.release_docs_gate._last_shipped_commit", return_value="origin/main")
def test_docs_check_base_is_the_last_shipped_commit_when_nothing_was_judged_since(
    mock_shipped: MagicMock, mock_judged: MagicMock
) -> None:
    assert _docs_check_base(MagicMock(), RepoSlug("o", "r"), DEV_COMMIT) == "origin/main"

    assert mock_judged.call_args.args[2:] == ("origin/main", DEV_COMMIT)


@patch(
    "functions.release_docs_gate._last_judged_commit",
    return_value=JudgedCommit(commit=JUDGED_COMMIT, pr_url=JUDGED_PR_URL),
)
@patch("functions.release_docs_gate._last_shipped_commit", return_value="origin/main")
def test_docs_check_base_is_the_commit_a_merged_docs_pull_request_settled(
    mock_shipped: MagicMock,
    mock_judged: MagicMock,
    capfd: pytest.CaptureFixture[str],
) -> None:
    # The rerun after the merge: the code up to the judged commit is settled,
    # so the check starts there instead of drawing new gaps from the same code.
    assert _docs_check_base(MagicMock(), RepoSlug("o", "r"), DEV_COMMIT) == JUDGED_COMMIT

    err = _unwrapped(capfd.readouterr().err)
    assert f"was already judged up to {JUDGED_COMMIT[:12]}" in err
    assert f"its gaps closed by {JUDGED_PR_URL}" in err


# --- the gate ---

_BLOCKING_CHANGE = RequiredChange(
    file="docs/x.mdx", change="document --verbose", severity="blocking"
)
_MINOR_CHANGE = RequiredChange(
    file="docs/y.mdx", change="reword the intro", severity="minor"
)


def _docs_gate(
    *,
    open_minor_docs_pr: bool = False,
    repo_root: Path = Path("/repo"),
    base: str = "origin/main",
) -> None:
    """Run the gate with placeholder GitHub handles, diffing from *base*."""
    with patch("functions.release_docs_gate._docs_check_base", return_value=base):
        verify_docs_gate(
            MagicMock(),
            RepoSlug(owner="test-owner", repo="test-repo"),
            DEV_COMMIT,
            repo_root,
            open_minor_docs_pr=open_minor_docs_pr,
        )


def _update_result(
    status: str, change: RequiredChange, summary: str = "updated"
) -> UpdateResult:
    return UpdateResult(
        results=(UpdateOutcome(file=change.file, change=change.change, status=status),),
        summary=summary,
    )


@patch("functions.release_docs_gate.update_docs")
@patch(
    "functions.release_docs_gate.check_docs",
    return_value=CheckResult(changes=()),
)
@patch("functions.release_docs_gate.has_changes_in_paths", return_value=False)
def test_docs_gate_passes_and_diffs_from_the_last_shipped_commit(
    mock_has_changes: MagicMock,
    mock_check: MagicMock,
    mock_update: MagicMock,
) -> None:
    _docs_gate(base="v0.29.0")

    # The base names the last shipped commit; the head is the dev commit
    # being released.
    mock_check.assert_called_once_with("v0.29.0", DEV_COMMIT)
    mock_update.assert_not_called()


@patch("functions.release_docs_gate._find_open_docs_sync_pr")
@patch("functions.release_docs_gate.update_docs")
@patch(
    "functions.release_docs_gate.check_docs",
    return_value=CheckResult(changes=(_MINOR_CHANGE,)),
)
@patch("functions.release_docs_gate.has_changes_in_paths", return_value=False)
def test_docs_gate_passes_on_minor_only_suggestions_left_alone(
    mock_has_changes: MagicMock,
    mock_check: MagicMock,
    mock_update: MagicMock,
    mock_find_pr: MagicMock,
    capfd: pytest.CaptureFixture[str],
) -> None:
    # Wording-level suggestions never stop a release. Without the optional
    # pull request they leave no trace: no lookup, no update run, no branch.
    _docs_gate(open_minor_docs_pr=False)

    mock_find_pr.assert_not_called()
    mock_update.assert_not_called()
    # Still printed for whoever reads the log.
    err = capfd.readouterr().err
    assert "reword the intro" in err
    assert "up to date" in err


@patch("functions.release_docs_gate._open_docs_pr")
@patch("functions.release_docs_gate._push_docs_sync_branch")
@patch("functions.release_docs_gate._find_open_docs_sync_pr", return_value=None)
@patch("functions.release_docs_gate.update_docs")
@patch(
    "functions.release_docs_gate.check_docs",
    return_value=CheckResult(changes=(_MINOR_CHANGE,)),
)
@patch("functions.release_docs_gate.has_changes_in_paths")
def test_docs_gate_opens_the_optional_polish_pr_and_continues(
    mock_has_changes: MagicMock,
    mock_check: MagicMock,
    mock_update: MagicMock,
    mock_find_pr: MagicMock,
    mock_push_branch: MagicMock,
    mock_open_pr: MagicMock,
    capfd: pytest.CaptureFixture[str],
) -> None:
    # The pull request opens on the side; the gate still returns normally so
    # the release goes on.
    mock_update.return_value = _update_result("implemented", _MINOR_CHANGE)
    # Clean at the gate's dirty check, dirty after the polish update.
    mock_has_changes.side_effect = [False, True]
    mock_open_pr.return_value = "https://github.com/test-owner/test-repo/pull/8"

    _docs_gate(open_minor_docs_pr=True, base="v0.29.0")

    mock_update.assert_called_once_with("v0.29.0", DEV_COMMIT, (_MINOR_CHANGE,))
    branch = f"auto/docs-polish-{DEV_COMMIT[:12]}"
    assert mock_find_pr.call_args.args[2] == branch
    mock_push_branch.assert_called_once_with(
        branch, Path("/repo/docs"), "docs: minor polish"
    )
    assert mock_open_pr.call_args.args[2] == branch
    assert "minor polish" in mock_open_pr.call_args.args[3]
    assert "reword the intro" in mock_open_pr.call_args.args[4]
    err = _unwrapped(capfd.readouterr().err)
    assert "pull/8" in err
    assert "does not block" in err


@patch("functions.release_docs_gate._open_docs_pr")
@patch("functions.release_docs_gate._push_docs_sync_branch")
@patch("functions.release_docs_gate._find_open_docs_sync_pr", return_value=None)
@patch("functions.release_docs_gate.update_docs")
@patch(
    "functions.release_docs_gate.check_docs",
    return_value=CheckResult(changes=(_MINOR_CHANGE,)),
)
@patch("functions.release_docs_gate.has_changes_in_paths", return_value=False)
def test_docs_gate_skips_the_polish_pr_when_the_update_changes_nothing(
    mock_has_changes: MagicMock,
    mock_check: MagicMock,
    mock_update: MagicMock,
    mock_find_pr: MagicMock,
    mock_push_branch: MagicMock,
    mock_open_pr: MagicMock,
    capfd: pytest.CaptureFixture[str],
) -> None:
    # A polish update that produces no docs edits leaves nothing to open; the
    # release goes on, since this path never fails a release.
    mock_update.return_value = _update_result(
        "already_covered", _MINOR_CHANGE, "nothing to polish"
    )

    _docs_gate(open_minor_docs_pr=True)

    mock_push_branch.assert_not_called()
    mock_open_pr.assert_not_called()
    err = capfd.readouterr().err
    assert "no pull request to open" in err
    assert "up to date" in err


@patch("functions.release_docs_gate.update_docs")
@patch(
    "functions.release_docs_gate._find_open_docs_sync_pr",
    return_value="https://github.com/test-owner/test-repo/pull/8",
)
@patch(
    "functions.release_docs_gate.check_docs",
    return_value=CheckResult(changes=(_MINOR_CHANGE,)),
)
@patch("functions.release_docs_gate.has_changes_in_paths", return_value=False)
def test_docs_gate_reports_the_polish_pr_already_open(
    mock_has_changes: MagicMock,
    mock_check: MagicMock,
    mock_find_pr: MagicMock,
    mock_update: MagicMock,
    capfd: pytest.CaptureFixture[str],
) -> None:
    # A polish pull request from an earlier attempt on this commit is
    # reported, not derived again.
    _docs_gate(open_minor_docs_pr=True)

    mock_update.assert_not_called()
    err = capfd.readouterr().err
    assert "pull/8" in err
    assert "up to date" in err


@patch("functions.release_docs_gate.check_docs")
@patch("functions.release_docs_gate.has_changes_in_paths", return_value=True)
def test_docs_gate_rejects_a_dirty_docs_tree_before_asking_claude(
    mock_has_changes: MagicMock,
    mock_check: MagicMock,
) -> None:
    # Those edits would be swept into the sync commit, so refuse up front rather
    # than after a multi-minute check.
    with pytest.raises(ReleaseError, match="'docs/' has uncommitted changes"):
        _docs_gate()

    mock_check.assert_not_called()


@patch("functions.release_docs_gate._open_docs_pr")
@patch("functions.release_docs_gate._push_docs_sync_branch")
@patch("functions.release_docs_gate._find_open_docs_sync_pr", return_value=None)
@patch("functions.release_docs_gate.update_docs")
@patch(
    "functions.release_docs_gate.check_docs",
    return_value=CheckResult(changes=(_BLOCKING_CHANGE,)),
)
@patch("functions.release_docs_gate.has_changes_in_paths")
def test_docs_gate_opens_a_pr_and_stops_the_release(
    mock_has_changes: MagicMock,
    mock_check: MagicMock,
    mock_update: MagicMock,
    mock_find_pr: MagicMock,
    mock_push_branch: MagicMock,
    mock_open_pr: MagicMock,
    capfd: pytest.CaptureFixture[str],
) -> None:
    mock_update.return_value = _update_result(
        "implemented", _BLOCKING_CHANGE, "edited 2 files"
    )
    # Clean before the update, dirty after it: claude rewrote the docs.
    mock_has_changes.side_effect = [False, True]
    mock_open_pr.return_value = "https://github.com/test-owner/test-repo/pull/7"

    with pytest.raises(SystemExit) as excinfo:
        _docs_gate(base="v0.29.0")

    assert excinfo.value.code == 1
    # The check and the update diff from the same base, the last shipped commit.
    mock_check.assert_called_once_with("v0.29.0", DEV_COMMIT)
    mock_update.assert_called_once_with("v0.29.0", DEV_COMMIT, (_BLOCKING_CHANGE,))
    # The branch is named after the release commit, so a retry on that commit
    # finds the pull request instead of producing a second one.
    branch = f"auto/docs-update-{DEV_COMMIT[:12]}"
    mock_push_branch.assert_called_once_with(
        branch, Path("/repo/docs"), "docs: sync with the code being released"
    )
    assert mock_open_pr.call_args.args[2] == branch
    # The log points at the pull request that has to merge first, and at the
    # new run that follows it.
    err = _unwrapped(capfd.readouterr().err)
    assert "pull/7" in err
    assert "start a new run of the release from 'dev'" in err


@patch("functions.release_docs_gate._open_docs_pr")
@patch("functions.release_docs_gate._push_docs_sync_branch")
@patch("functions.release_docs_gate._find_open_docs_sync_pr", return_value=None)
@patch("functions.release_docs_gate.update_docs")
@patch(
    "functions.release_docs_gate.check_docs",
    return_value=CheckResult(changes=(_MINOR_CHANGE, _BLOCKING_CHANGE)),
)
@patch("functions.release_docs_gate.has_changes_in_paths")
def test_docs_gate_feeds_only_blocking_changes_to_the_update_and_pr(
    mock_has_changes: MagicMock,
    mock_check: MagicMock,
    mock_update: MagicMock,
    mock_find_pr: MagicMock,
    mock_push_branch: MagicMock,
    mock_open_pr: MagicMock,
    capfd: pytest.CaptureFixture[str],
) -> None:
    # A mixed verdict: the blocking gap drives the update and the pull
    # request, and the minor one is only printed. The optional polish pull
    # request belongs to minor-only verdicts, so it is not opened even when
    # asked for.
    mock_update.return_value = _update_result("implemented", _BLOCKING_CHANGE)
    mock_has_changes.side_effect = [False, True]
    mock_open_pr.return_value = "https://github.com/test-owner/test-repo/pull/7"

    with pytest.raises(SystemExit):
        _docs_gate(open_minor_docs_pr=True)

    mock_update.assert_called_once_with(
        "origin/main", DEV_COMMIT, (_BLOCKING_CHANGE,)
    )
    body = mock_open_pr.call_args.args[4]
    assert "docs/x.mdx" in body
    assert "reword the intro" not in body
    assert [c.args[2] for c in mock_find_pr.call_args_list] == [
        f"auto/docs-update-{DEV_COMMIT[:12]}"
    ]
    assert "reword the intro" in capfd.readouterr().err


@patch("functions.release_docs_gate._push_docs_sync_branch")
@patch("functions.release_docs_gate.update_docs")
@patch(
    "functions.release_docs_gate._find_open_docs_sync_pr",
    return_value="https://github.com/test-owner/test-repo/pull/7",
)
@patch(
    "functions.release_docs_gate.check_docs",
    return_value=CheckResult(changes=(_BLOCKING_CHANGE,)),
)
@patch("functions.release_docs_gate.has_changes_in_paths", return_value=False)
def test_docs_gate_stops_on_the_pr_an_earlier_attempt_opened(
    mock_has_changes: MagicMock,
    mock_check: MagicMock,
    mock_find_pr: MagicMock,
    mock_update: MagicMock,
    mock_push_branch: MagicMock,
    capfd: pytest.CaptureFixture[str],
) -> None:
    # Re-running the release on a commit whose docs pull request is still open
    # must point at that pull request, not spend another Claude run deriving a
    # branch that would then have to replace the one under review.
    with pytest.raises(SystemExit) as excinfo:
        _docs_gate()

    assert excinfo.value.code == 1
    mock_find_pr.assert_called_once()
    assert mock_find_pr.call_args.args[2] == f"auto/docs-update-{DEV_COMMIT[:12]}"
    mock_update.assert_not_called()
    mock_push_branch.assert_not_called()
    assert "pull/7" in capfd.readouterr().err


@patch("functions.release_docs_gate._open_docs_pr")
@patch("functions.release_docs_gate._push_docs_sync_branch")
@patch("functions.release_docs_gate._find_open_docs_sync_pr", return_value=None)
@patch("functions.release_docs_gate.update_docs")
@patch(
    "functions.release_docs_gate.check_docs",
    return_value=CheckResult(changes=(_BLOCKING_CHANGE,)),
)
@patch("functions.release_docs_gate.has_changes_in_paths", return_value=False)
def test_docs_gate_continues_when_the_updater_finds_gaps_already_covered(
    mock_has_changes: MagicMock,
    mock_check: MagicMock,
    mock_update: MagicMock,
    mock_find_pr: MagicMock,
    mock_push_branch: MagicMock,
    mock_open_pr: MagicMock,
    capfd: pytest.CaptureFixture[str],
) -> None:
    # The check claimed a gap; the updater read the docs and verified each
    # reported gap is already documented. That is the check being noisy, not
    # the docs being stale, and it must not block the release.
    mock_update.return_value = _update_result(
        "already_covered", _BLOCKING_CHANGE, "everything already documented"
    )

    _docs_gate()

    mock_push_branch.assert_not_called()
    mock_open_pr.assert_not_called()
    assert "already documented" in _unwrapped(capfd.readouterr().err)


@patch("functions.release_docs_gate._push_docs_sync_branch")
@patch("functions.release_docs_gate._find_open_docs_sync_pr", return_value=None)
@patch("functions.release_docs_gate.update_docs")
@patch(
    "functions.release_docs_gate.check_docs",
    return_value=CheckResult(changes=(_BLOCKING_CHANGE,)),
)
@patch("functions.release_docs_gate.has_changes_in_paths", return_value=False)
def test_docs_gate_raises_when_the_update_claims_edits_but_changed_nothing(
    mock_has_changes: MagicMock,
    mock_check: MagicMock,
    mock_update: MagicMock,
    mock_find_pr: MagicMock,
    mock_push_branch: MagicMock,
) -> None:
    # An "implemented" claim with a clean docs tree is a malfunctioning
    # updater, not a noisy check; passing silently would make the gate
    # unfalsifiable, so the release stops with the manual route spelled out.
    mock_update.return_value = _update_result("implemented", _BLOCKING_CHANGE)

    with pytest.raises(ReleaseError, match="nothing changed there"):
        _docs_gate()

    mock_push_branch.assert_not_called()


# --- the docs pull requests ---


@patch("functions.release_docs_gate.switch_branch")
@patch("functions.release_docs_gate.push_branch")
@patch("functions.release_docs_gate.commit_paths")
@patch("functions.release_docs_gate.switch_to_new_branch")
def test_push_docs_sync_branch_pushes_without_force_and_returns_to_dev(
    mock_switch_new: MagicMock,
    mock_commit: MagicMock,
    mock_push: MagicMock,
    mock_switch: MagicMock,
) -> None:
    _push_docs_sync_branch(
        "auto/docs-update-abc",
        Path("/repo/docs"),
        "docs: sync with the code being released",
    )

    mock_switch_new.assert_called_once_with("auto/docs-update-abc")
    mock_commit.assert_called_once_with(
        [Path("/repo/docs")], "docs: sync with the code being released"
    )
    # push_branch is the plain, non-forced push: a docs branch that has already
    # been published is never overwritten.
    mock_push.assert_called_once_with(
        "origin", "auto/docs-update-abc", "auto/docs-update-abc"
    )
    mock_switch.assert_called_once_with("dev")


@patch("functions.release_docs_gate.switch_branch")
@patch("functions.release_docs_gate.push_branch", side_effect=ReleaseError("non-fast-forward"))
@patch("functions.release_docs_gate.commit_paths")
@patch("functions.release_docs_gate.switch_to_new_branch")
def test_push_docs_sync_branch_explains_a_rejected_push_and_returns_to_dev(
    mock_switch_new: MagicMock,
    mock_commit: MagicMock,
    mock_push: MagicMock,
    mock_switch: MagicMock,
) -> None:
    # A rejection means a branch was left behind by an attempt whose pull
    # request is gone; say how to clear it instead of overwriting it. And a
    # failed push must never strand the working tree on the throwaway branch.
    with pytest.raises(ReleaseError) as excinfo:
        _push_docs_sync_branch(
            "auto/docs-update-abc", Path("/repo/docs"), "docs: minor polish"
        )

    message = str(excinfo.value)
    assert "non-fast-forward" in message
    assert "git push origin --delete auto/docs-update-abc" in message
    mock_switch.assert_called_once_with("dev")


@patch("functions.release_docs_gate.github_api")
def test_open_docs_pr_creates_a_pr_against_dev(mock_api: MagicMock) -> None:
    slug = RepoSlug(owner="test-owner", repo="test-repo")
    mock_api.return_value = {
        "html_url": "https://github.com/test-owner/test-repo/pull/9"
    }

    url = _open_docs_pr(
        MagicMock(),
        slug,
        "auto/docs-update-abc",
        "docs: sync with the code being released (abc)",
        f"body mentioning docs/x.mdx at {DEV_COMMIT}",
    )

    assert url == "https://github.com/test-owner/test-repo/pull/9"
    payload = mock_api.call_args.kwargs["json_data"]
    assert payload["head"] == "auto/docs-update-abc"
    assert payload["base"] == "dev"
    assert payload["title"] == "docs: sync with the code being released (abc)"
    assert "docs/x.mdx" in payload["body"]
    assert DEV_COMMIT in payload["body"]


@patch("functions.release_docs_gate.github_api")
def test_find_open_docs_sync_pr_queries_the_branch_into_dev(
    mock_api: MagicMock,
) -> None:
    mock_api.return_value = [
        {"html_url": "https://github.com/test-owner/test-repo/pull/4"}
    ]

    url = _find_open_docs_sync_pr(
        MagicMock(),
        RepoSlug(owner="test-owner", repo="test-repo"),
        "auto/docs-update-abc",
    )

    assert url == "https://github.com/test-owner/test-repo/pull/4"
    query = mock_api.call_args.args[2]
    assert "head=test-owner:auto/docs-update-abc" in query
    assert "base=dev" in query
    assert "state=open" in query


@patch("functions.release_docs_gate.github_api", return_value=[])
def test_find_open_docs_sync_pr_returns_none_when_there_is_no_pr(
    mock_api: MagicMock,
) -> None:
    assert (
        _find_open_docs_sync_pr(
            MagicMock(),
            RepoSlug(owner="test-owner", repo="test-repo"),
            "auto/docs-update-abc",
        )
        is None
    )

