"""Tests for functions.release_line."""

from __future__ import annotations

from unittest.mock import MagicMock, patch

import pytest

from functions.cli import ReleaseError
from functions.github import RepoSlug
from functions.release_line import latest_release_tag, verify_release_branch_state

DEV_COMMIT = "1111111111111111111111111111111111111111"
MAIN_COMMIT = "2222222222222222222222222222222222222222"


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

