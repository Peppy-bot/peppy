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


class TestNeedCmd:
    def test_exists(self) -> None:
        path = need_cmd("git")
        assert path
        assert "git" in path

    def test_missing(self) -> None:
        with pytest.raises(ReleaseError, match="missing required command: nonexistent_cmd_xyz"):
            need_cmd("nonexistent_cmd_xyz")


class TestPrompt:
    def test_with_user_input(self) -> None:
        with patch("functions.cli.Prompt.ask", return_value="my_answer"):
            result = prompt("Enter value")
        assert result == "my_answer"

    def test_with_default(self) -> None:
        with patch("functions.cli.Prompt.ask", return_value="default_val"):
            result = prompt("Enter value", default="default_val")
        assert result == "default_val"

    def test_empty_input_returns_empty(self) -> None:
        with patch("functions.cli.Prompt.ask", return_value=""):
            result = prompt("Enter value")
        assert result == ""

    def test_none_input_returns_empty(self) -> None:
        with patch("functions.cli.Prompt.ask", return_value=None):
            result = prompt("Enter value")
        assert result == ""


class TestPromptYn:
    def test_yes(self) -> None:
        with patch("functions.cli.Confirm.ask", return_value=True):
            assert prompt_yn("Continue?") is True

    def test_no(self) -> None:
        with patch("functions.cli.Confirm.ask", return_value=False):
            assert prompt_yn("Continue?") is False

    def test_default_yes(self) -> None:
        with patch("functions.cli.Confirm.ask", return_value=True) as mock_ask:
            prompt_yn("Continue?", default_yes=True)
            mock_ask.assert_called_once_with("Continue?", default=True)

    def test_default_no(self) -> None:
        with patch("functions.cli.Confirm.ask", return_value=False) as mock_ask:
            prompt_yn("Continue?", default_yes=False)
            mock_ask.assert_called_once_with("Continue?", default=False)


class TestValidateReleaseEnvironment:
    def test_missing_token(self) -> None:
        with patch.dict(os.environ, {}, clear=True):
            with pytest.raises(ReleaseError, match="GITHUB_PEPPY_RELEASE_TOKEN"):
                validate_release_environment(required_commands=())

    def test_valid_token(self) -> None:
        with patch.dict(os.environ, {"GITHUB_PEPPY_RELEASE_TOKEN": "test-token"}):
            token = validate_release_environment(required_commands=())
        assert token == "test-token"

    def test_no_token_required(self) -> None:
        with patch.dict(os.environ, {}, clear=True):
            token = validate_release_environment(
                required_commands=(), require_token=False
            )
        assert token == ""

    def test_missing_command(self) -> None:
        with patch.dict(os.environ, {"GITHUB_PEPPY_RELEASE_TOKEN": "test-token"}):
            with pytest.raises(ReleaseError, match="missing required command"):
                validate_release_environment(
                    required_commands=("nonexistent_cmd_xyz",)
                )

    def test_whitespace_token_is_empty(self) -> None:
        with patch.dict(os.environ, {"GITHUB_PEPPY_RELEASE_TOKEN": "  "}):
            with pytest.raises(ReleaseError, match="GITHUB_PEPPY_RELEASE_TOKEN"):
                validate_release_environment(required_commands=())


class TestRunWithErrorHandling:
    def test_catches_release_error(self) -> None:
        def failing() -> None:
            raise ReleaseError("something went wrong")

        with pytest.raises(SystemExit) as exc_info:
            run_with_error_handling(failing)
        assert exc_info.value.code == 1

    def test_catches_keyboard_interrupt(self) -> None:
        def interrupted() -> None:
            raise KeyboardInterrupt

        with pytest.raises(SystemExit) as exc_info:
            run_with_error_handling(interrupted)
        assert exc_info.value.code == 130

    def test_success_no_exit(self) -> None:
        def success() -> None:
            pass

        # Should not raise
        run_with_error_handling(success)
