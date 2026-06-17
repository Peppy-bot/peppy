"""Tests for functions.repo module."""

from __future__ import annotations

from unittest.mock import MagicMock, patch

import pytest

from functions.cli import ReleaseError
from functions.repo import get_commit_subjects


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
