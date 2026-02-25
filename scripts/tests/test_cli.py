"""Tests for functions.cli module."""

from __future__ import annotations

import os
from unittest.mock import patch

import pytest

from functions.cli import (
    ReleaseError,
    need_cmd,
    prompt,
    prompt_yn,
    run_with_error_handling,
    validate_release_environment,
)


def test_need_cmd_exists() -> None:
    path = need_cmd("git")
    assert path
    assert "git" in path


def test_need_cmd_missing() -> None:
    with pytest.raises(
        ReleaseError, match="missing required command: nonexistent_cmd_xyz"
    ):
        need_cmd("nonexistent_cmd_xyz")


def test_prompt_with_user_input() -> None:
    with patch("functions.cli.Prompt.ask", return_value="my_answer"):
        result = prompt("Enter value")
    assert result == "my_answer"


def test_prompt_with_default() -> None:
    with patch("functions.cli.Prompt.ask", return_value="default_val"):
        result = prompt("Enter value", default="default_val")
    assert result == "default_val"


def test_prompt_empty_input_returns_empty() -> None:
    with patch("functions.cli.Prompt.ask", return_value=""):
        result = prompt("Enter value")
    assert result == ""


def test_prompt_none_input_returns_empty() -> None:
    with patch("functions.cli.Prompt.ask", return_value=None):
        result = prompt("Enter value")
    assert result == ""


def test_prompt_yn_yes() -> None:
    with patch("functions.cli.Confirm.ask", return_value=True):
        assert prompt_yn("Continue?") is True


def test_prompt_yn_no() -> None:
    with patch("functions.cli.Confirm.ask", return_value=False):
        assert prompt_yn("Continue?") is False


def test_prompt_yn_default_yes() -> None:
    with patch("functions.cli.Confirm.ask", return_value=True) as mock_ask:
        prompt_yn("Continue?", default_yes=True)
        mock_ask.assert_called_once_with("Continue?", default=True)


def test_prompt_yn_default_no() -> None:
    with patch("functions.cli.Confirm.ask", return_value=False) as mock_ask:
        prompt_yn("Continue?", default_yes=False)
        mock_ask.assert_called_once_with("Continue?", default=False)


def test_validate_release_environment_missing_token() -> None:
    with patch.dict(os.environ, {}, clear=True):
        with pytest.raises(ReleaseError, match="GITHUB_PEPPY_RELEASE_TOKEN"):
            validate_release_environment(required_commands=())


def test_validate_release_environment_valid_token() -> None:
    with patch.dict(os.environ, {"GITHUB_PEPPY_RELEASE_TOKEN": "test-token"}):
        token = validate_release_environment(required_commands=())
    assert token == "test-token"


def test_validate_release_environment_no_token_required() -> None:
    with patch.dict(os.environ, {}, clear=True):
        token = validate_release_environment(required_commands=(), require_token=False)
    assert token == ""


def test_validate_release_environment_missing_command() -> None:
    with patch.dict(os.environ, {"GITHUB_PEPPY_RELEASE_TOKEN": "test-token"}):
        with pytest.raises(ReleaseError, match="missing required command"):
            validate_release_environment(required_commands=("nonexistent_cmd_xyz",))


def test_validate_release_environment_whitespace_token_is_empty() -> None:
    with patch.dict(os.environ, {"GITHUB_PEPPY_RELEASE_TOKEN": "  "}):
        with pytest.raises(ReleaseError, match="GITHUB_PEPPY_RELEASE_TOKEN"):
            validate_release_environment(required_commands=())


def test_run_with_error_handling_catches_release_error() -> None:
    def failing() -> None:
        raise ReleaseError("something went wrong")

    with pytest.raises(SystemExit) as exc_info:
        run_with_error_handling(failing)
    assert exc_info.value.code == 1


def test_run_with_error_handling_catches_keyboard_interrupt() -> None:
    def interrupted() -> None:
        raise KeyboardInterrupt

    with pytest.raises(SystemExit) as exc_info:
        run_with_error_handling(interrupted)
    assert exc_info.value.code == 130
