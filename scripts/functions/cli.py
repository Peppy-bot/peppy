"""Interactive CLI utilities: fatal errors, command checks, prompts, and error handling."""

from __future__ import annotations

import os
import platform
import shutil
import sys
from collections.abc import Callable, Sequence

from rich.console import Console
from rich.prompt import Confirm, Prompt

console = Console(stderr=True)

# Env var the release scripts read the GitHub release token from. The release
# workflow sets it to an installation token of the peppy-release-bot GitHub
# App, made by the job that runs the stage. It lasts one hour.
RELEASE_TOKEN_ENV = "PEPPY_RELEASE_TOKEN"

# Env var the release scripts read the job's own GITHUB_TOKEN from. It only
# reads, and it lasts as long as the job, so a stage that waits longer than
# the hour of a release token reads with it.
JOB_TOKEN_ENV = "PEPPY_JOB_TOKEN"

RELEASE_TRIPLES: tuple[str, ...] = (
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
)


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


def _require_token(env_var: str) -> str:
    token = os.environ.get(env_var, "").strip()
    if not token:
        raise ReleaseError(f"{env_var} env var is required")
    return token


def require_release_token() -> str:
    """Return the GitHub release token held in the RELEASE_TOKEN_ENV env var.

    Raises ReleaseError if it is unset or blank.
    """
    return _require_token(RELEASE_TOKEN_ENV)


def require_job_token() -> str:
    """Return the job's own GitHub token held in the JOB_TOKEN_ENV env var.

    Raises ReleaseError if it is unset or blank.
    """
    return _require_token(JOB_TOKEN_ENV)


def validate_release_environment(required_commands: Sequence[str]) -> str:
    """Check what a release needs before it starts: the token and the commands.

    Returns the release token. Raises ReleaseError if any check fails.
    """
    token = require_release_token()

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


def detect_platform() -> tuple[str, str]:
    """Return (os_name, arch) for the current host.

    Examples: ('Darwin', 'arm64'), ('Linux', 'x86_64').
    """
    return (platform.system(), platform.machine())


def is_macos_arm64() -> bool:
    """Return True if running on macOS Apple Silicon."""
    os_name, arch = detect_platform()
    return os_name == "Darwin" and arch == "arm64"


def is_linux() -> bool:
    """Return True if running on Linux."""
    os_name, _ = detect_platform()
    return os_name == "Linux"


def get_native_triple() -> str:
    """Return the Rust target triple for the current host."""
    os_name, arch = detect_platform()
    if os_name == "Darwin" and arch == "arm64":
        return "aarch64-apple-darwin"
    if os_name == "Linux" and arch in ("x86_64", "amd64"):
        return "x86_64-unknown-linux-gnu"
    if os_name == "Linux" and arch in ("aarch64", "arm64"):
        return "aarch64-unknown-linux-gnu"
    raise ReleaseError(f"unsupported platform: {os_name} {arch}")


def get_targets_for_platform() -> list[str]:
    """Return the list of target triples to build for the current platform.

    On macOS ARM64: every release triple (RELEASE_TRIPLES).
    On Linux: only the native triple.
    """
    if is_macos_arm64():
        return list(RELEASE_TRIPLES)
    if is_linux():
        return [get_native_triple()]
    raise ReleaseError("unsupported platform (requires macOS ARM64 or Linux)")
