"""Tests for functions.release_line."""

from __future__ import annotations

from unittest.mock import MagicMock, patch

import pytest

from functions.cli import ReleaseError
from functions.github import RepoSlug
from functions.release_line import (
    latest_release_tag,
    update_release_branch,
    verify_publish_branch_state,
    verify_release_branch_state,
)

DEV_COMMIT = "1111111111111111111111111111111111111111"
MAIN_COMMIT = "2222222222222222222222222222222222222222"
REMOTE_DEV_COMMIT = "3333333333333333333333333333333333333333"
RELEASE_COMMIT = "4444444444444444444444444444444444444444"


# --- release branch state ---


def _commit_resolver(commits: dict[str, str]):
    """Return a get_commit stub resolving the given revisions."""
    return lambda rev: commits[rev]


def _ancestry_resolver(ancestry: dict[tuple[str, str], bool]):
    """Return an is_ancestor stub answering the given (ancestor, descendant) pairs."""
    return lambda ancestor, descendant: ancestry[(ancestor, descendant)]


@patch("functions.release_line.get_current_branch", return_value="main")
def test_verify_release_branch_state_rejects_other_branch(
    mock_branch: MagicMock,
) -> None:
    with pytest.raises(ReleaseError, match="releases are cut from 'dev' only"):
        verify_release_branch_state()


@patch("functions.release_line.get_current_branch", return_value=None)
def test_verify_release_branch_state_rejects_detached_head(
    mock_branch: MagicMock,
) -> None:
    with pytest.raises(ReleaseError, match="HEAD is on a detached commit"):
        verify_release_branch_state()


@patch("functions.release_line.is_ancestor", return_value=True)
@patch("functions.release_line.get_commit")
@patch("functions.release_line.fetch_remote_branches")
@patch("functions.release_line.get_current_branch", return_value="dev")
def test_verify_release_branch_state_returns_the_dev_commit(
    mock_branch: MagicMock,
    mock_fetch: MagicMock,
    mock_get_commit: MagicMock,
    mock_is_ancestor: MagicMock,
) -> None:
    mock_get_commit.side_effect = _commit_resolver(
        {"HEAD": DEV_COMMIT, "origin/dev": DEV_COMMIT, "origin/main": MAIN_COMMIT}
    )

    assert verify_release_branch_state() == DEV_COMMIT

    # Both remote-tracking refs are refreshed before they are compared.
    mock_fetch.assert_called_once_with("origin", ("dev", "main"))
    mock_is_ancestor.assert_called_once_with(MAIN_COMMIT, DEV_COMMIT)


@patch("functions.release_line.is_ancestor")
@patch("functions.release_line.get_commit")
@patch("functions.release_line.fetch_remote_branches")
@patch("functions.release_line.get_current_branch", return_value="dev")
def test_verify_release_branch_state_rejects_dev_behind_remote(
    mock_branch: MagicMock,
    mock_fetch: MagicMock,
    mock_get_commit: MagicMock,
    mock_is_ancestor: MagicMock,
) -> None:
    remote_dev = "3333333333333333333333333333333333333333"
    mock_get_commit.side_effect = _commit_resolver(
        {"HEAD": DEV_COMMIT, "origin/dev": remote_dev, "origin/main": MAIN_COMMIT}
    )
    mock_is_ancestor.side_effect = _ancestry_resolver({(DEV_COMMIT, remote_dev): True})

    with pytest.raises(ReleaseError, match="'dev' is behind origin/dev"):
        verify_release_branch_state()


@patch("functions.release_line.is_ancestor")
@patch("functions.release_line.get_commit")
@patch("functions.release_line.fetch_remote_branches")
@patch("functions.release_line.get_current_branch", return_value="dev")
def test_verify_release_branch_state_rejects_unpushed_dev_commits(
    mock_branch: MagicMock,
    mock_fetch: MagicMock,
    mock_get_commit: MagicMock,
    mock_is_ancestor: MagicMock,
) -> None:
    remote_dev = "3333333333333333333333333333333333333333"
    mock_get_commit.side_effect = _commit_resolver(
        {"HEAD": DEV_COMMIT, "origin/dev": remote_dev, "origin/main": MAIN_COMMIT}
    )
    mock_is_ancestor.side_effect = _ancestry_resolver(
        {(DEV_COMMIT, remote_dev): False, (remote_dev, DEV_COMMIT): True}
    )

    with pytest.raises(ReleaseError, match="commits that are not on origin/dev"):
        verify_release_branch_state()


@patch("functions.release_line.is_ancestor")
@patch("functions.release_line.get_commit")
@patch("functions.release_line.fetch_remote_branches")
@patch("functions.release_line.get_current_branch", return_value="dev")
def test_verify_release_branch_state_rejects_diverged_dev(
    mock_branch: MagicMock,
    mock_fetch: MagicMock,
    mock_get_commit: MagicMock,
    mock_is_ancestor: MagicMock,
) -> None:
    remote_dev = "3333333333333333333333333333333333333333"
    mock_get_commit.side_effect = _commit_resolver(
        {"HEAD": DEV_COMMIT, "origin/dev": remote_dev, "origin/main": MAIN_COMMIT}
    )
    mock_is_ancestor.side_effect = _ancestry_resolver(
        {(DEV_COMMIT, remote_dev): False, (remote_dev, DEV_COMMIT): False}
    )

    with pytest.raises(ReleaseError, match="'dev' and origin/dev have diverged"):
        verify_release_branch_state()


@patch("functions.release_line.is_ancestor", return_value=False)
@patch("functions.release_line.get_commit")
@patch("functions.release_line.fetch_remote_branches")
@patch("functions.release_line.get_current_branch", return_value="dev")
def test_verify_release_branch_state_rejects_main_ahead_of_dev(
    mock_branch: MagicMock,
    mock_fetch: MagicMock,
    mock_get_commit: MagicMock,
    mock_is_ancestor: MagicMock,
) -> None:
    # main carries a commit dev does not have, so it cannot fast-forward.
    mock_get_commit.side_effect = _commit_resolver(
        {"HEAD": DEV_COMMIT, "origin/dev": DEV_COMMIT, "origin/main": MAIN_COMMIT}
    )

    with pytest.raises(ReleaseError, match="'main' cannot fast-forward to it"):
        verify_release_branch_state()


# --- the branch state of a publish ---


@patch("functions.release_line.get_current_branch", return_value=None)
def test_update_release_branch_rejects_detached_head(mock_branch: MagicMock) -> None:
    with pytest.raises(ReleaseError, match="HEAD is on a detached commit"):
        update_release_branch()


@patch("functions.release_line.fast_forward_current_branch")
@patch("functions.release_line.is_ancestor")
@patch("functions.release_line.get_commit")
@patch("functions.release_line.fetch_remote_branches")
@patch("functions.release_line.get_current_branch", return_value="dev")
def test_update_release_branch_leaves_dev_at_origin_dev_as_it_is(
    mock_branch: MagicMock,
    mock_fetch: MagicMock,
    mock_get_commit: MagicMock,
    mock_is_ancestor: MagicMock,
    mock_fast_forward: MagicMock,
) -> None:
    mock_get_commit.side_effect = _commit_resolver(
        {"HEAD": DEV_COMMIT, "origin/dev": DEV_COMMIT}
    )

    assert update_release_branch() == DEV_COMMIT

    # Both are fetched: the publish reads origin/main next.
    mock_fetch.assert_called_once_with("origin", ("dev", "main"))
    mock_fast_forward.assert_not_called()


@patch("functions.release_line.fast_forward_current_branch")
@patch("functions.release_line.is_ancestor")
@patch("functions.release_line.get_commit")
@patch("functions.release_line.fetch_remote_branches")
@patch("functions.release_line.get_current_branch", return_value="dev")
def test_update_release_branch_fast_forwards_dev_behind_origin_dev(
    mock_branch: MagicMock,
    mock_fetch: MagicMock,
    mock_get_commit: MagicMock,
    mock_is_ancestor: MagicMock,
    mock_fast_forward: MagicMock,
) -> None:
    # A merge into dev landed after the checkout.
    mock_get_commit.side_effect = _commit_resolver(
        {"HEAD": DEV_COMMIT, "origin/dev": REMOTE_DEV_COMMIT}
    )
    mock_is_ancestor.side_effect = _ancestry_resolver(
        {(DEV_COMMIT, REMOTE_DEV_COMMIT): True}
    )

    assert update_release_branch() == REMOTE_DEV_COMMIT

    mock_fast_forward.assert_called_once_with(REMOTE_DEV_COMMIT)


@pytest.mark.parametrize(
    ("remote_holds_dev", "message"),
    [
        # The push of the notes would publish them.
        (True, "commits that are not on origin/dev"),
        (False, "'dev' and origin/dev have diverged"),
    ],
    ids=["unpushed-commits", "diverged"],
)
@patch("functions.release_line.fast_forward_current_branch")
@patch("functions.release_line.is_ancestor")
@patch("functions.release_line.get_commit")
@patch("functions.release_line.fetch_remote_branches")
@patch("functions.release_line.get_current_branch", return_value="dev")
def test_update_release_branch_refuses_dev_commits_origin_dev_lacks(
    mock_branch: MagicMock,
    mock_fetch: MagicMock,
    mock_get_commit: MagicMock,
    mock_is_ancestor: MagicMock,
    mock_fast_forward: MagicMock,
    remote_holds_dev: bool,
    message: str,
) -> None:
    mock_get_commit.side_effect = _commit_resolver(
        {"HEAD": DEV_COMMIT, "origin/dev": REMOTE_DEV_COMMIT}
    )
    mock_is_ancestor.side_effect = _ancestry_resolver(
        {
            (DEV_COMMIT, REMOTE_DEV_COMMIT): False,
            (REMOTE_DEV_COMMIT, DEV_COMMIT): remote_holds_dev,
        }
    )

    with pytest.raises(ReleaseError, match=message):
        update_release_branch()

    mock_fast_forward.assert_not_called()


@patch("functions.release_line.is_ancestor")
@patch("functions.release_line.update_release_branch", return_value=DEV_COMMIT)
def test_verify_publish_branch_state_accepts_dev_that_took_merges_since(
    mock_update: MagicMock, mock_is_ancestor: MagicMock
) -> None:
    mock_is_ancestor.side_effect = _ancestry_resolver({(RELEASE_COMMIT, DEV_COMMIT): True})

    assert verify_publish_branch_state(RELEASE_COMMIT) == DEV_COMMIT


@patch("functions.release_line.is_ancestor")
@patch("functions.release_line.update_release_branch", return_value=DEV_COMMIT)
def test_verify_publish_branch_state_refuses_dev_without_the_release_commit(
    mock_update: MagicMock, mock_is_ancestor: MagicMock
) -> None:
    mock_is_ancestor.side_effect = _ancestry_resolver(
        {(RELEASE_COMMIT, DEV_COMMIT): False}
    )

    with pytest.raises(ReleaseError) as excinfo:
        verify_publish_branch_state(RELEASE_COMMIT)

    message = " ".join(str(excinfo.value).split())
    assert f"'dev' is at {DEV_COMMIT[:12]}, which does not hold {RELEASE_COMMIT[:12]}" in (
        message
    )
    assert "This publish stops before it writes anything" in message


# --- the latest release tag ---

LATEST_TAG = "v0.29.0"


@patch("functions.release_line.fetch_tag")
@patch(
    "functions.release_line.get_latest_release",
    return_value={"tag_name": LATEST_TAG},
)
def test_latest_release_tag_fetches_the_tag_before_answering(
    mock_latest: MagicMock, mock_fetch: MagicMock
) -> None:
    assert latest_release_tag(MagicMock(), RepoSlug("o", "r")) == LATEST_TAG

    # The tag can be newer than the checkout, so it is fetched before it is read.
    mock_fetch.assert_called_once_with("origin", LATEST_TAG)


@patch("functions.release_line.fetch_tag")
@patch("functions.release_line.get_latest_release", return_value=None)
def test_latest_release_tag_is_none_without_a_published_release(
    mock_latest: MagicMock, mock_fetch: MagicMock
) -> None:
    assert latest_release_tag(MagicMock(), RepoSlug("o", "r")) is None

    mock_fetch.assert_not_called()

