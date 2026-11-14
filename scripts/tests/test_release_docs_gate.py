"""Tests for functions.release_docs_gate."""

from __future__ import annotations

from collections.abc import Callable, Iterator, Sequence
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from functions.cli import ReleaseError
from functions.concepts_video import (
    ChangedSection,
    MissingName,
    SegmentFindings,
    SegmentOutcome,
    SegmentStatus,
    VideoCheck,
    VideoUpdate,
)
from functions.docs import (
    EM_DASH,
    CheckResult,
    RequiredChange,
    UpdateOutcome,
    UpdateResult,
)
from functions.github import RepoSlug
from functions.release_docs_gate import (
    DocsPull,
    JudgedCommit,
    _comment_on_docs_pr,
    _docs_check_base,
    _docs_sync_pr_body,
    _find_open_docs_pr,
    _judged_commit_of,
    _last_judged_commit,
    _last_shipped_commit,
    _open_docs_pr,
    _push_docs_branch,
    _render_steps_comment,
    verify_docs_gate,
)

from .helpers import ancestry_resolver, unwrapped

DEV_COMMIT = "1111111111111111111111111111111111111111"

# The gate never asks anything: every prompt fails the test.
pytestmark = pytest.mark.usefixtures("no_prompts")


# --- the last shipped commit ---

LATEST_TAG = "v0.29.0"


@patch("functions.release_docs_gate.is_ancestor")
@patch("functions.release_docs_gate.latest_release_tag", return_value=None)
def test_last_shipped_commit_is_origin_main_without_a_published_release(
    mock_tag: MagicMock, mock_is_ancestor: MagicMock
) -> None:
    assert (
        _last_shipped_commit(MagicMock(), RepoSlug("o", "r"), DEV_COMMIT)
        == "origin/main"
    )

    mock_is_ancestor.assert_not_called()


@patch("functions.release_docs_gate.is_ancestor")
@patch("functions.release_docs_gate.latest_release_tag", return_value=LATEST_TAG)
def test_last_shipped_commit_is_origin_main_once_aligned_to_the_latest_release(
    mock_tag: MagicMock, mock_is_ancestor: MagicMock
) -> None:
    # main sits one notes commit past the tag, as a finished release leaves it.
    mock_is_ancestor.side_effect = ancestry_resolver(
        {(LATEST_TAG, "origin/main"): True}
    )

    assert (
        _last_shipped_commit(MagicMock(), RepoSlug("o", "r"), DEV_COMMIT)
        == "origin/main"
    )


@patch("functions.release_docs_gate.is_ancestor")
@patch("functions.release_docs_gate.latest_release_tag", return_value=LATEST_TAG)
def test_last_shipped_commit_is_the_latest_release_tag_when_main_lags_it(
    mock_tag: MagicMock,
    mock_is_ancestor: MagicMock,
    capfd: pytest.CaptureFixture[str],
) -> None:
    # A release published but never aligned: the tag is past main, on the way
    # to the commit being released.
    mock_is_ancestor.side_effect = ancestry_resolver(
        {
            (LATEST_TAG, "origin/main"): False,
            ("origin/main", LATEST_TAG): True,
            (LATEST_TAG, DEV_COMMIT): True,
        }
    )

    assert (
        _last_shipped_commit(MagicMock(), RepoSlug("o", "r"), DEV_COMMIT) == LATEST_TAG
    )

    err = unwrapped(capfd.readouterr().err)
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
    mock_is_ancestor.side_effect = ancestry_resolver(ancestry)

    assert (
        _last_shipped_commit(MagicMock(), RepoSlug("o", "r"), DEV_COMMIT)
        == "origin/main"
    )


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
@patch(
    "functions.release_docs_gate.find_commit",
    _known_commits(JUDGED_COMMIT, JUDGED_MERGE),
)
def test_judged_commit_of_a_merged_docs_sync_pull_request(
    mock_is_ancestor: MagicMock,
) -> None:
    mock_is_ancestor.side_effect = ancestry_resolver(_ON_THE_RELEASE_LINE)

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
@patch(
    "functions.release_docs_gate.find_commit",
    _known_commits(JUDGED_COMMIT, JUDGED_MERGE),
)
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
@patch(
    "functions.release_docs_gate.find_commit",
    _known_commits(JUDGED_COMMIT, JUDGED_MERGE),
)
def test_judged_commit_of_is_none_off_the_way_to_the_release(
    mock_is_ancestor: MagicMock, ancestry: dict[tuple[str, str], bool]
) -> None:
    mock_is_ancestor.side_effect = ancestry_resolver(ancestry)

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
    mock_is_ancestor.side_effect = ancestry_resolver(
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
def test_docs_check_base_is_the_last_shipped_commit_when_nothing_was_judged_since(
    mock_judged: MagicMock,
) -> None:
    assert (
        _docs_check_base(MagicMock(), RepoSlug("o", "r"), "origin/main", DEV_COMMIT)
        == "origin/main"
    )

    assert mock_judged.call_args.args[2:] == ("origin/main", DEV_COMMIT)


@patch(
    "functions.release_docs_gate._last_judged_commit",
    return_value=JudgedCommit(commit=JUDGED_COMMIT, pr_url=JUDGED_PR_URL),
)
def test_docs_check_base_is_the_commit_a_merged_docs_pull_request_settled(
    mock_judged: MagicMock,
    capfd: pytest.CaptureFixture[str],
) -> None:
    # The rerun after the merge: the code up to the judged commit is settled,
    # so the check starts there instead of drawing new gaps from the same code.
    assert (
        _docs_check_base(MagicMock(), RepoSlug("o", "r"), "origin/main", DEV_COMMIT)
        == JUDGED_COMMIT
    )

    err = unwrapped(capfd.readouterr().err)
    assert f"was already judged up to {JUDGED_COMMIT[:12]}" in err
    assert f"its gaps closed by {JUDGED_PR_URL}" in err


# --- the gate ---

REPO_ROOT = Path("/repo")
SLUG = RepoSlug(owner="test-owner", repo="test-repo")
DOCS_PAGE = "docs/src/content/docs/x.mdx"
TRANSCRIPT = "docs/concepts_video/transcript.toml"
PULL = DocsPull(number=7, url="https://github.com/test-owner/test-repo/pull/7")
SYNC_BRANCH = f"auto/docs-update-{DEV_COMMIT[:12]}"
# Where the stand-in for the worktree of the last shipped commit is.
LAST_RELEASE_TREE = Path("/last-release")
CONTENT_SHA256 = "c" * 64

_BLOCKING_CHANGE = RequiredChange(
    file="docs/x.mdx", change="document --verbose", severity="blocking"
)
_MINOR_CHANGE = RequiredChange(
    file="docs/y.mdx", change="reword the intro", severity="minor"
)

# A segment of the video whose docs section changed since the last release.
_CHANGED_SECTION = SegmentFindings(
    segment_id="launchers",
    changed_sections=(
        ChangedSection(page="quickstart.mdx", section="Quickstart", diff="-old\n+new"),
    ),
    missing_sections=(),
    missing_names=(),
)
# A segment of the video that shows a name its docs sections no longer hold.
_MISSING_NAME = SegmentFindings(
    segment_id="stack",
    changed_sections=(),
    missing_sections=(),
    missing_names=(MissingName(where="sentence `launch`", name="--with"),),
)


def _video_check(
    *segments: SegmentFindings, upload_is_current: bool = True
) -> VideoCheck:
    return VideoCheck(
        segments=segments,
        content_sha256=CONTENT_SHA256,
        upload_is_current=upload_is_current,
    )


def _video_update(
    *outcomes: tuple[SegmentFindings, str, str], for_the_reviewer: str = ""
) -> VideoUpdate:
    """An update report: one (segment, status, reason) per reported segment."""
    return VideoUpdate(
        outcomes=tuple(
            SegmentOutcome(
                segment_id=segment.segment_id,
                status=SegmentStatus(status),
                reason=reason,
            )
            for segment, status, reason in outcomes
        ),
        for_the_reviewer=for_the_reviewer,
    )


def _docs_gate(
    *,
    open_minor_docs_pr: bool = False,
    skip_docs_check: bool = False,
    skip_video_check: bool = False,
    shipped: str = "origin/main",
    base: str | None = None,
) -> None:
    """Run the gate with placeholder GitHub handles: the last shipped commit
    is *shipped*, and the docs check diffs from *base* (*shipped* when None)."""
    with (
        patch("functions.release_docs_gate._last_shipped_commit", return_value=shipped),
        patch(
            "functions.release_docs_gate._docs_check_base",
            return_value=shipped if base is None else base,
        ),
    ):
        verify_docs_gate(
            MagicMock(),
            SLUG,
            DEV_COMMIT,
            REPO_ROOT,
            open_minor_docs_pr=open_minor_docs_pr,
            skip_docs_check=skip_docs_check,
            skip_video_check=skip_video_check,
        )


def _update_result(
    status: str, change: RequiredChange, summary: str = "updated"
) -> UpdateResult:
    return UpdateResult(
        results=(UpdateOutcome(file=change.file, change=change.change, status=status),),
        summary=summary,
    )


class FakeTree:
    """The files of the checkout that hold uncommitted changes."""

    def __init__(self) -> None:
        self.changed: set[Path] = set()

    def edit(self, path: str) -> None:
        self.changed.add(REPO_ROOT / path)

    def has_changes(self, paths: Sequence[Path]) -> bool:
        return any(
            changed == path or path in changed.parents
            for changed in self.changed
            for path in paths
        )


def _editing(tree: FakeTree, path: str, result: object) -> Callable[..., object]:
    """A stand-in for a Claude update that edits *path* and reports *result*."""

    def run(*args: object, **kwargs: object) -> object:
        tree.edit(path)
        return result

    return run


@dataclass(frozen=True)
class GateCollaborators:
    """The stand-ins of what the gate calls, for a test to set or check."""

    tree: FakeTree
    check: MagicMock
    update: MagicMock
    find_pr: MagicMock
    push_branch: MagicMock
    open_pr: MagicMock
    comment: MagicMock
    worktree: MagicMock
    check_video: MagicMock
    update_video: MagicMock
    record_sections: MagicMock


@pytest.fixture
def gate() -> Iterator[GateCollaborators]:
    """Stand in for every collaborator of the gate: a clean docs tree, a docs
    check that finds no gap, a concepts video that agrees with the docs and no
    open docs pull request, unless a test sets otherwise."""
    tree = FakeTree()

    @contextmanager
    def last_release_tree(commit: str) -> Iterator[Path]:
        yield LAST_RELEASE_TREE

    with (
        patch(
            "functions.release_docs_gate.has_changes_in_paths",
            side_effect=tree.has_changes,
        ),
        patch(
            "functions.release_docs_gate.check_docs",
            return_value=CheckResult(changes=()),
        ) as check,
        patch("functions.release_docs_gate.update_docs") as update,
        patch(
            "functions.release_docs_gate._find_open_docs_pr", return_value=None
        ) as find_pr,
        patch("functions.release_docs_gate._push_docs_branch") as push_branch,
        patch(
            "functions.release_docs_gate._open_docs_pr", return_value=PULL
        ) as open_pr,
        patch("functions.release_docs_gate._comment_on_docs_pr") as comment,
        patch(
            "functions.release_docs_gate.detached_worktree",
            side_effect=last_release_tree,
        ) as worktree,
        patch(
            "functions.release_docs_gate.check_video", return_value=_video_check()
        ) as check_video,
        patch("functions.release_docs_gate.update_video") as update_video,
        patch("functions.release_docs_gate.record_sections") as record_sections,
    ):
        yield GateCollaborators(
            tree=tree,
            check=check,
            update=update,
            find_pr=find_pr,
            push_branch=push_branch,
            open_pr=open_pr,
            comment=comment,
            worktree=worktree,
            check_video=check_video,
            update_video=update_video,
            record_sections=record_sections,
        )


def test_docs_gate_passes_and_diffs_from_the_last_shipped_commit(
    gate: GateCollaborators,
) -> None:
    _docs_gate(shipped="v0.29.0")

    # The base names the last shipped commit; the head is the dev commit
    # being released.
    gate.check.assert_called_once_with("v0.29.0", DEV_COMMIT)
    gate.update.assert_not_called()


def test_docs_gate_passes_on_minor_only_suggestions_left_alone(
    gate: GateCollaborators, capfd: pytest.CaptureFixture[str]
) -> None:
    gate.check.return_value = CheckResult(changes=(_MINOR_CHANGE,))

    # Wording-level suggestions never stop a release. Without the optional
    # pull request they leave no trace: no lookup, no update run, no branch.
    _docs_gate(open_minor_docs_pr=False)

    gate.find_pr.assert_not_called()
    gate.update.assert_not_called()
    # Still printed for whoever reads the log.
    err = capfd.readouterr().err
    assert "reword the intro" in err
    assert "up to date" in err


def test_docs_gate_opens_the_optional_polish_pr_and_continues(
    gate: GateCollaborators, capfd: pytest.CaptureFixture[str]
) -> None:
    gate.check.return_value = CheckResult(changes=(_MINOR_CHANGE,))
    # The pull request opens on the side; the gate still returns normally so
    # the release goes on.
    gate.update.side_effect = _editing(
        gate.tree, DOCS_PAGE, _update_result("implemented", _MINOR_CHANGE)
    )
    gate.open_pr.return_value = DocsPull(
        number=8, url="https://github.com/test-owner/test-repo/pull/8"
    )

    _docs_gate(open_minor_docs_pr=True, shipped="v0.29.0")

    gate.update.assert_called_once_with("v0.29.0", DEV_COMMIT, (_MINOR_CHANGE,))
    branch = f"auto/docs-polish-{DEV_COMMIT[:12]}"
    assert gate.find_pr.call_args.args[2] == branch
    gate.push_branch.assert_called_once_with(
        branch, Path("/repo/docs"), "docs: minor polish"
    )
    assert gate.open_pr.call_args.args[2] == branch
    assert "minor polish" in gate.open_pr.call_args.args[3]
    assert "reword the intro" in gate.open_pr.call_args.args[4]
    err = unwrapped(capfd.readouterr().err)
    assert "pull/8" in err
    assert "does not block" in err


def test_docs_gate_skips_the_polish_pr_when_the_update_changes_nothing(
    gate: GateCollaborators, capfd: pytest.CaptureFixture[str]
) -> None:
    gate.check.return_value = CheckResult(changes=(_MINOR_CHANGE,))
    # A polish update that produces no docs edits leaves nothing to open; the
    # release goes on, since this path never fails a release.
    gate.update.return_value = _update_result(
        "already_covered", _MINOR_CHANGE, "nothing to polish"
    )

    _docs_gate(open_minor_docs_pr=True)

    gate.push_branch.assert_not_called()
    gate.open_pr.assert_not_called()
    err = capfd.readouterr().err
    assert "no pull request to open" in err
    assert "up to date" in err


def test_docs_gate_reports_the_polish_pr_already_open(
    gate: GateCollaborators, capfd: pytest.CaptureFixture[str]
) -> None:
    gate.check.return_value = CheckResult(changes=(_MINOR_CHANGE,))
    gate.find_pr.return_value = "https://github.com/test-owner/test-repo/pull/8"

    # A polish pull request from an earlier attempt on this commit is
    # reported, not derived again.
    _docs_gate(open_minor_docs_pr=True)

    gate.update.assert_not_called()
    err = capfd.readouterr().err
    assert "pull/8" in err
    assert "up to date" in err


def test_docs_gate_rejects_a_dirty_docs_tree_before_asking_claude(
    gate: GateCollaborators,
) -> None:
    gate.tree.edit(DOCS_PAGE)

    # Those edits would be swept into the sync commit, so refuse up front rather
    # than after a multi-minute check.
    with pytest.raises(ReleaseError, match="'docs/' has uncommitted changes"):
        _docs_gate()

    gate.check.assert_not_called()
    gate.check_video.assert_not_called()


def test_docs_gate_opens_a_pr_and_stops_the_release(
    gate: GateCollaborators, capfd: pytest.CaptureFixture[str]
) -> None:
    gate.check.return_value = CheckResult(changes=(_BLOCKING_CHANGE,))
    # Claude rewrote the docs.
    gate.update.side_effect = _editing(
        gate.tree,
        DOCS_PAGE,
        _update_result("implemented", _BLOCKING_CHANGE, "edited 2 files"),
    )

    with pytest.raises(SystemExit) as excinfo:
        _docs_gate(shipped="v0.29.0")

    assert excinfo.value.code == 1
    # The check and the update diff from the same base, the last shipped commit.
    gate.check.assert_called_once_with("v0.29.0", DEV_COMMIT)
    gate.update.assert_called_once_with("v0.29.0", DEV_COMMIT, (_BLOCKING_CHANGE,))
    # The branch is named after the release commit, so a retry on that commit
    # finds the pull request instead of producing a second one.
    gate.push_branch.assert_called_once_with(
        SYNC_BRANCH, Path("/repo/docs"), "docs: sync with the code being released"
    )
    assert gate.open_pr.call_args.args[2] == SYNC_BRANCH
    # The video agrees with the edited docs, so the pull request holds the
    # docs alone: no hash is recorded and nobody is asked for a render.
    gate.record_sections.assert_not_called()
    gate.comment.assert_not_called()
    # The log points at the pull request that has to merge first, and at the
    # new run that follows it.
    err = unwrapped(capfd.readouterr().err)
    assert "pull/7" in err
    assert "start a new run of the release from 'dev'" in err


def test_docs_gate_feeds_only_blocking_changes_to_the_update_and_pr(
    gate: GateCollaborators, capfd: pytest.CaptureFixture[str]
) -> None:
    gate.check.return_value = CheckResult(changes=(_MINOR_CHANGE, _BLOCKING_CHANGE))
    # A mixed verdict: the blocking gap drives the update and the pull
    # request, and the minor one is only printed. The optional polish pull
    # request belongs to minor-only verdicts, so it is not opened even when
    # asked for.
    gate.update.side_effect = _editing(
        gate.tree, DOCS_PAGE, _update_result("implemented", _BLOCKING_CHANGE)
    )

    with pytest.raises(SystemExit):
        _docs_gate(open_minor_docs_pr=True)

    gate.update.assert_called_once_with("origin/main", DEV_COMMIT, (_BLOCKING_CHANGE,))
    body = gate.open_pr.call_args.args[4]
    assert "docs/x.mdx" in body
    assert "reword the intro" not in body
    assert [c.args[2] for c in gate.find_pr.call_args_list] == [SYNC_BRANCH]
    assert "reword the intro" in capfd.readouterr().err


def test_docs_gate_stops_on_the_pr_an_earlier_attempt_opened(
    gate: GateCollaborators, capfd: pytest.CaptureFixture[str]
) -> None:
    gate.check.return_value = CheckResult(changes=(_BLOCKING_CHANGE,))
    gate.find_pr.return_value = "https://github.com/test-owner/test-repo/pull/7"

    # Re-running the release on a commit whose docs pull request is still open
    # must point at that pull request, not spend another Claude run deriving a
    # branch that would then have to replace the one under review.
    with pytest.raises(SystemExit) as excinfo:
        _docs_gate()

    assert excinfo.value.code == 1
    gate.find_pr.assert_called_once()
    assert gate.find_pr.call_args.args[2] == SYNC_BRANCH
    gate.update.assert_not_called()
    gate.check_video.assert_not_called()
    gate.push_branch.assert_not_called()
    assert "pull/7" in capfd.readouterr().err


def test_docs_gate_continues_when_the_updater_finds_gaps_already_covered(
    gate: GateCollaborators, capfd: pytest.CaptureFixture[str]
) -> None:
    gate.check.return_value = CheckResult(changes=(_BLOCKING_CHANGE,))
    # The check claimed a gap; the updater read the docs and verified each
    # reported gap is already documented. That is the check being noisy, not
    # the docs being stale, and it must not block the release.
    gate.update.return_value = _update_result(
        "already_covered", _BLOCKING_CHANGE, "everything already documented"
    )

    _docs_gate()

    gate.push_branch.assert_not_called()
    gate.open_pr.assert_not_called()
    # The video is still read against the docs the release ships.
    gate.check_video.assert_called_once()
    assert "already documented" in unwrapped(capfd.readouterr().err)


def test_docs_gate_raises_when_the_update_claims_edits_but_changed_nothing(
    gate: GateCollaborators,
) -> None:
    gate.check.return_value = CheckResult(changes=(_BLOCKING_CHANGE,))
    # An "implemented" claim with a clean docs tree is a malfunctioning
    # updater, not a noisy check; passing silently would make the gate
    # unfalsifiable, so the release stops with the manual route spelled out.
    gate.update.return_value = _update_result("implemented", _BLOCKING_CHANGE)

    with pytest.raises(ReleaseError, match="nothing changed there"):
        _docs_gate()

    gate.push_branch.assert_not_called()


# --- the concepts video in the gate ---


def test_docs_gate_reads_the_video_against_the_docs_of_the_last_shipped_commit(
    gate: GateCollaborators, capfd: pytest.CaptureFixture[str]
) -> None:
    # The docs check starts from the commit a merged pull request settled, but
    # the video is read against what the last release shipped: its transcript
    # records the sections a pull request settled.
    _docs_gate(shipped="v0.29.0", base=JUDGED_COMMIT)

    gate.worktree.assert_called_once_with("v0.29.0")
    gate.check_video.assert_called_once_with(
        REPO_ROOT, LAST_RELEASE_TREE / "docs/src/content/docs"
    )
    # A video that agrees costs no Claude run and leaves no trace.
    gate.update_video.assert_not_called()
    gate.record_sections.assert_not_called()
    gate.open_pr.assert_not_called()
    assert "The concepts video agrees with the docs" in capfd.readouterr().err


def test_docs_gate_leaves_the_video_alone_when_asked(gate: GateCollaborators) -> None:
    _docs_gate(skip_video_check=True)

    gate.check.assert_called_once()
    gate.worktree.assert_not_called()
    gate.check_video.assert_not_called()


def test_docs_gate_opens_a_pr_for_a_video_the_docs_no_longer_agree_with(
    gate: GateCollaborators, capfd: pytest.CaptureFixture[str]
) -> None:
    # A docs section the video rests on changed, and Claude corrected a
    # sentence of the transcript: the video on YouTube no longer shows it.
    gate.check_video.side_effect = [
        _video_check(_CHANGED_SECTION),
        _video_check(upload_is_current=False),
    ]
    gate.update_video.side_effect = _editing(
        gate.tree,
        TRANSCRIPT,
        _video_update((_CHANGED_SECTION, "changed", "The flag has a new name.")),
    )

    with pytest.raises(SystemExit) as excinfo:
        _docs_gate()

    assert excinfo.value.code == 1
    gate.update_video.assert_called_once_with(REPO_ROOT, (_CHANGED_SECTION,))
    # The transcript records the sections that were read, then is checked as
    # the pull request holds it, with no baseline.
    gate.record_sections.assert_called_once_with(REPO_ROOT)
    assert gate.check_video.call_args_list[1].args == (REPO_ROOT, None)
    gate.push_branch.assert_called_once_with(
        SYNC_BRANCH,
        Path("/repo/docs"),
        "docs: sync the concepts video with the docs being released",
    )
    title, body = gate.open_pr.call_args.args[3:5]
    assert title == (
        f"docs: sync the concepts video with the docs being released "
        f"({DEV_COMMIT[:12]})"
    )
    assert "Automated update of the concepts video" in body
    assert "- Segment `launchers` (changed): The flag has a new name." in body
    assert "Gaps the check reported" not in body
    assert "The video on YouTube is not the video of this transcript" in body
    # The reviewer gets the steps to render, upload and give the link.
    pull, comment = gate.comment.call_args.args[2:4]
    assert pull == PULL
    assert "pixi run build" in comment
    assert CONTENT_SHA256 in comment
    assert "pull/7" in capfd.readouterr().err


def test_docs_gate_puts_the_docs_and_the_video_in_one_pull_request(
    gate: GateCollaborators,
) -> None:
    gate.check.return_value = CheckResult(changes=(_BLOCKING_CHANGE,))
    gate.update.side_effect = _editing(
        gate.tree, DOCS_PAGE, _update_result("implemented", _BLOCKING_CHANGE)
    )
    # The edits that closed the docs gap changed a section the video rests on.
    gate.check_video.side_effect = [
        _video_check(_CHANGED_SECTION),
        _video_check(upload_is_current=False),
    ]
    gate.update_video.side_effect = _editing(
        gate.tree,
        TRANSCRIPT,
        _video_update((_CHANGED_SECTION, "changed", "The flag has a new name.")),
    )

    with pytest.raises(SystemExit):
        _docs_gate()

    # One branch, one commit of `docs/`, one pull request, under the subject
    # of the docs sync.
    gate.push_branch.assert_called_once_with(
        SYNC_BRANCH, Path("/repo/docs"), "docs: sync with the code being released"
    )
    gate.open_pr.assert_called_once()
    body = gate.open_pr.call_args.args[4]
    assert "- `docs/x.mdx`: document --verbose" in body
    assert "- Segment `launchers` (changed): The flag has a new name." in body
    gate.comment.assert_called_once()


def test_docs_gate_records_the_sections_of_a_video_that_the_docs_edits_left_correct(
    gate: GateCollaborators,
) -> None:
    gate.check.return_value = CheckResult(changes=(_BLOCKING_CHANGE,))
    gate.update.side_effect = _editing(
        gate.tree, DOCS_PAGE, _update_result("implemented", _BLOCKING_CHANGE)
    )
    gate.check_video.side_effect = [_video_check(_CHANGED_SECTION), _video_check()]
    gate.update_video.return_value = _video_update(
        (_CHANGED_SECTION, "still_correct", "The video does not show this flag.")
    )

    with pytest.raises(SystemExit):
        _docs_gate()

    # The pull request of the docs records the sections the video was read
    # against, so the rerun after its merge does not read them again. The
    # video itself did not change, so nobody is asked for a render.
    gate.record_sections.assert_called_once_with(REPO_ROOT)
    body = gate.open_pr.call_args.args[4]
    assert (
        "- Segment `launchers` (still correct): The video does not show this flag."
        in body
    )
    assert "The video on YouTube" not in body
    gate.comment.assert_not_called()


def test_docs_gate_continues_when_the_changed_sections_leave_the_video_correct(
    gate: GateCollaborators, capfd: pytest.CaptureFixture[str]
) -> None:
    gate.check_video.return_value = _video_check(_CHANGED_SECTION)
    gate.update_video.return_value = _video_update(
        (_CHANGED_SECTION, "still_correct", "The video does not show this flag.")
    )

    # A section that changed and left the video correct stops nothing, and no
    # pull request is opened only to record that it was read: the next
    # release reads the video against what this one ships.
    _docs_gate()

    gate.record_sections.assert_not_called()
    gate.push_branch.assert_not_called()
    gate.open_pr.assert_not_called()
    err = unwrapped(capfd.readouterr().err)
    assert "launchers (still correct): The video does not show this flag." in err
    assert "leave the concepts video correct" in err
    assert "up to date" in err


def test_docs_gate_stops_when_the_video_names_what_the_docs_do_not_hold(
    gate: GateCollaborators,
) -> None:
    gate.check_video.return_value = _video_check(_MISSING_NAME)
    gate.update_video.return_value = _video_update(
        (_MISSING_NAME, "still_correct", "The flag is in another section.")
    )

    # A name the docs no longer hold cannot be left as it is: with no edit to
    # put in a pull request, the release stops and says what to do by hand.
    with pytest.raises(ReleaseError) as excinfo:
        _docs_gate()

    message = str(excinfo.value)
    assert (
        "Segment `stack`, sentence `launch`: no section that the segment names "
        "holds `--with`." in message
    )
    assert "--skip-video-check" in message
    gate.open_pr.assert_not_called()


def test_docs_gate_lists_what_the_video_update_left_for_the_reviewer(
    gate: GateCollaborators,
) -> None:
    gate.check_video.side_effect = [
        _video_check(_CHANGED_SECTION, _MISSING_NAME),
        _video_check(_MISSING_NAME, upload_is_current=False),
    ]
    gate.update_video.side_effect = _editing(
        gate.tree,
        TRANSCRIPT,
        _video_update(
            (_CHANGED_SECTION, "changed", "The flag has a new name."),
            (_MISSING_NAME, "still_correct", "The composition shows the flag."),
            for_the_reviewer="The composition of `stack` shows `--with`.",
        ),
    )

    with pytest.raises(SystemExit):
        _docs_gate()

    body = gate.open_pr.call_args.args[4]
    left = body[body.index("Left for the reviewer:") :]
    assert (
        "- Segment `stack`, sentence `launch`: no section that the segment "
        "names holds `--with`." in left
    )
    assert "- The composition of `stack` shows `--with`." in left


def test_docs_gate_raises_when_the_video_update_claims_an_edit_but_made_none(
    gate: GateCollaborators,
) -> None:
    gate.check_video.return_value = _video_check(_CHANGED_SECTION)
    # A "changed" claim with an untouched transcript is a malfunctioning
    # update: letting it pass would ship a video its own judge calls stale.
    gate.update_video.return_value = _video_update(
        (_CHANGED_SECTION, "changed", "The flag has a new name.")
    )

    with pytest.raises(ReleaseError, match="transcript.toml' did not change"):
        _docs_gate()

    gate.open_pr.assert_not_called()


def test_docs_gate_stops_on_the_open_pr_before_reading_the_video_again(
    gate: GateCollaborators, capfd: pytest.CaptureFixture[str]
) -> None:
    gate.check_video.return_value = _video_check(_CHANGED_SECTION)
    gate.find_pr.return_value = "https://github.com/test-owner/test-repo/pull/7"

    with pytest.raises(SystemExit) as excinfo:
        _docs_gate()

    assert excinfo.value.code == 1
    assert gate.find_pr.call_args.args[2] == SYNC_BRANCH
    gate.update_video.assert_not_called()
    assert "pull/7" in capfd.readouterr().err


def test_docs_gate_without_the_docs_check_names_its_branch_after_the_video(
    gate: GateCollaborators,
) -> None:
    gate.check_video.side_effect = [
        _video_check(_CHANGED_SECTION),
        _video_check(upload_is_current=False),
    ]
    gate.update_video.side_effect = _editing(
        gate.tree,
        TRANSCRIPT,
        _video_update((_CHANGED_SECTION, "changed", "The flag has a new name.")),
    )

    with pytest.raises(SystemExit):
        _docs_gate(skip_docs_check=True)

    # A merged `auto/docs-update-` pull request vouches that the docs check
    # judged the code up to its commit. This run did not check the docs, so
    # its branch must not carry that prefix.
    gate.check.assert_not_called()
    branch = f"auto/video-update-{DEV_COMMIT[:12]}"
    assert gate.find_pr.call_args.args[2] == branch
    assert gate.push_branch.call_args.args[0] == branch
    assert gate.open_pr.call_args.args[2] == branch


def test_docs_gate_without_the_docs_check_passes_on_a_video_that_agrees(
    gate: GateCollaborators, capfd: pytest.CaptureFixture[str]
) -> None:
    _docs_gate(skip_docs_check=True)

    gate.check.assert_not_called()
    gate.check_video.assert_called_once()
    # Nothing vouches for the docs, so the log does not call them up to date.
    assert "up to date" not in capfd.readouterr().err


# --- the pull request of the gate ---


def test_docs_sync_pr_body_of_docs_gaps_alone_names_the_gaps() -> None:
    body = _docs_sync_pr_body(DEV_COMMIT, (_BLOCKING_CHANGE,))

    assert body.splitlines() == [
        "Automated docs update produced by the release script "
        "(`scripts/functions/docs.py`), which found `docs/` lagging the code at "
        "the commit the release was being cut from.",
        "",
        f"Release commit: {DEV_COMMIT}",
        "",
        "Gaps the check reported:",
        "",
        "- `docs/x.mdx`: document --verbose",
        "",
        "Merge this into `dev`, then re-run the release.",
    ]


def test_render_steps_comment_gives_the_build_the_upload_and_the_text_box() -> None:
    comment = _render_steps_comment(SLUG, SYNC_BRANCH, CONTENT_SHA256)

    # The branch to build, the commands, and the two ways to give the link.
    assert f"git switch {SYNC_BRANCH}" in comment
    assert "cd docs/concepts_video" in comment
    for command in (
        "pixi run build",
        "pixi run test",
        "pixi run layout",
        "pixi run motion",
        "pixi run record-upload <link>",
    ):
        assert command in comment
    # The text box: GitHub's editor of the record, on the branch of the pull
    # request, with the hash the record has to hold.
    assert (
        f"https://github.com/test-owner/test-repo/edit/{SYNC_BRANCH}/"
        "docs/concepts_video/upload.json" in comment
    )
    assert CONTENT_SHA256 in comment
    assert (
        f"https://github.com/test-owner/test-repo/blob/{SYNC_BRANCH}/"
        "docs/concepts_video/README.md" in comment
    )
    # A comment of the release never holds an em-dash.
    assert EM_DASH not in comment


@patch("functions.release_docs_gate.github_api")
def test_comment_on_docs_pr_posts_to_the_issue_of_the_pull_request(
    mock_api: MagicMock,
) -> None:
    client = MagicMock()

    _comment_on_docs_pr(client, SLUG, PULL, "the steps")

    mock_api.assert_called_once_with(
        client,
        "POST",
        "https://api.github.com/repos/test-owner/test-repo/issues/7/comments",
        json_data={"body": "the steps"},
    )


# --- the docs pull requests ---


@patch("functions.release_docs_gate.switch_branch")
@patch("functions.release_docs_gate.push_branch")
@patch("functions.release_docs_gate.commit_paths")
@patch("functions.release_docs_gate.switch_to_new_branch")
def test_push_docs_branch_pushes_without_force_and_returns_to_dev(
    mock_switch_new: MagicMock,
    mock_commit: MagicMock,
    mock_push: MagicMock,
    mock_switch: MagicMock,
) -> None:
    _push_docs_branch(
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
@patch(
    "functions.release_docs_gate.push_branch",
    side_effect=ReleaseError("non-fast-forward"),
)
@patch("functions.release_docs_gate.commit_paths")
@patch("functions.release_docs_gate.switch_to_new_branch")
def test_push_docs_branch_explains_a_rejected_push_and_returns_to_dev(
    mock_switch_new: MagicMock,
    mock_commit: MagicMock,
    mock_push: MagicMock,
    mock_switch: MagicMock,
) -> None:
    # A rejection means a branch was left behind by an attempt whose pull
    # request is gone; say how to clear it instead of overwriting it. And a
    # failed push must never strand the working tree on the throwaway branch.
    with pytest.raises(ReleaseError) as excinfo:
        _push_docs_branch(
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
        "number": 9,
        "html_url": "https://github.com/test-owner/test-repo/pull/9",
    }

    pull = _open_docs_pr(
        MagicMock(),
        slug,
        "auto/docs-update-abc",
        "docs: sync with the code being released (abc)",
        f"body mentioning docs/x.mdx at {DEV_COMMIT}",
    )

    assert pull == DocsPull(
        number=9, url="https://github.com/test-owner/test-repo/pull/9"
    )
    payload = mock_api.call_args.kwargs["json_data"]
    assert payload["head"] == "auto/docs-update-abc"
    assert payload["base"] == "dev"
    assert payload["title"] == "docs: sync with the code being released (abc)"
    assert "docs/x.mdx" in payload["body"]
    assert DEV_COMMIT in payload["body"]


@pytest.mark.parametrize(
    "response",
    [
        # No number: the comment for the reviewer could not name the pull request.
        {"html_url": "https://github.com/test-owner/test-repo/pull/9"},
        {"number": 9},
        [],
    ],
)
@patch("functions.release_docs_gate.github_api")
def test_open_docs_pr_refuses_a_response_that_names_no_pull_request(
    mock_api: MagicMock, response: object
) -> None:
    mock_api.return_value = response

    with pytest.raises(ReleaseError, match="unexpected GitHub API response"):
        _open_docs_pr(MagicMock(), SLUG, "auto/docs-update-abc", "title", "body")


@patch("functions.release_docs_gate.github_api")
def test_find_open_docs_pr_queries_the_branch_into_dev(
    mock_api: MagicMock,
) -> None:
    mock_api.return_value = [
        {"html_url": "https://github.com/test-owner/test-repo/pull/4"}
    ]

    url = _find_open_docs_pr(
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
def test_find_open_docs_pr_returns_none_when_there_is_no_pr(
    mock_api: MagicMock,
) -> None:
    assert (
        _find_open_docs_pr(
            MagicMock(),
            RepoSlug(owner="test-owner", repo="test-repo"),
            "auto/docs-update-abc",
        )
        is None
    )
