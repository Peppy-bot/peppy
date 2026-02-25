"""Interactive CLI utilities: fatal errors, command checks, prompts, and error handling."""

from __future__ import annotations

import os
import shutil
import sys
from collections.abc import Callable, Sequence

from rich.console import Console
from rich.prompt import Confirm, Prompt

console = Console(stderr=True)


class ReleaseError(Exception):
    """Raised when the release process encounters a fatal error.

    All error messages are formatted for human consumption (printed to stderr).
    Entry points catch this and call sys.exit(1).
    """


def need_cmd(cmd: str) -> str:
    """Verify that an external command is available on PATH.

    Returns the resolved path to the command.
    Raises ReleaseError if the command is not found.
    """
    path = shutil.which(cmd)
    if path is None:
        raise ReleaseError(f"missing required command: {cmd}")
    return path


def prompt(label: str, default: str = "") -> str:
    """Prompt the user for a string value, with optional default shown in brackets.

    Returns the user's input, or the default if they press enter.
    """
    return Prompt.ask(label, default=default or None) or ""


def prompt_yn(label: str, default_yes: bool = False) -> bool:
    """Prompt the user for a yes/no answer.

    Returns True for yes, False for no. Empty input returns the default.
    """
    return Confirm.ask(label, default=default_yes)


def validate_release_environment(
    required_commands: Sequence[str] = ("git", "cargo", "rustc"),
    *,
    require_token: bool = True,
) -> str:
    """Validate the release environment: check token and required commands.

    When require_token is False (for --local mode), skips token validation
    and returns an empty string for the token.

    Returns the validated token string (empty if not required).
    Raises ReleaseError if any check fails.
    """
    token = ""
    if require_token:
        token = os.environ.get("GITHUB_PEPPY_RELEASE_TOKEN", "").strip()
        if not token:
            raise ReleaseError("GITHUB_PEPPY_RELEASE_TOKEN env var is required")

    for cmd in required_commands:
        need_cmd(cmd)

    return token


def run_with_error_handling(fn: Callable[[], None]) -> None:
    """Run a function with standard error handling for release scripts.

    Catches ReleaseError and KeyboardInterrupt, exits with appropriate codes.
    """
    try:
        fn()
    except ReleaseError as e:
        console.print(f"[red]error:[/red] {e}")
        sys.exit(1)
    except KeyboardInterrupt:
        console.print("\n[dim]Aborted.[/dim]")
        sys.exit(130)
