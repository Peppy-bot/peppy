"""Runs the HyperFrames command line of the project.

Each run keeps the caches of HyperFrames inside the project, sends no usage
data, and makes the voice with the Python of the pixi environment.
"""

import json
import os
from pathlib import Path
import subprocess
import sys

from tools import paths

EXECUTABLE = paths.NODE_MODULES / ".bin" / "hyperframes"
LINT_FAULT_SEVERITIES = ("error", "warning")
# The end of the code of each lint finding that says that a media file is absent.
ABSENT_MEDIA_CODE_SUFFIX = "_src_not_found"


class LintFailed(Exception):
    """The lint of HyperFrames found a fault in the compiled project."""


def environment() -> dict[str, str]:
    return {
        **os.environ,
        "HYPERFRAMES_CACHE_DIR": str(paths.HYPERFRAMES_CACHE),
        "HYPERFRAMES_PYTHON": sys.executable,
        "HYPERFRAMES_NO_TELEMETRY": "1",
        "HYPERFRAMES_NO_UPDATE_CHECK": "1",
        "DO_NOT_TRACK": "1",
    }


def run(arguments: list[str], extra_environment: dict[str, str] | None = None) -> None:
    """Run one command, with its output on the terminal. Fail when it fails."""
    subprocess.run(
        [str(EXECUTABLE), *arguments],
        cwd=paths.PROJECT_ROOT,
        env={**environment(), **(extra_environment or {})},
        check=True,
    )


def output_of(arguments: list[str]) -> str:
    """Run one command and give what it prints. Fail when it fails."""
    completed = subprocess.run(
        [str(EXECUTABLE), *arguments],
        cwd=paths.PROJECT_ROOT,
        env=environment(),
        check=True,
        capture_output=True,
        text=True,
    )
    return completed.stdout


def lint(project: Path, media_are_present: bool) -> None:
    """Lint a compiled project. Fail on each error and on each warning.

    Without the voice and the footage, which the repository does not hold,
    the finding that a media file is absent is not a fault.
    """
    completed = subprocess.run(
        [str(EXECUTABLE), "lint", str(project), "--json"],
        cwd=paths.PROJECT_ROOT,
        env=environment(),
        capture_output=True,
        text=True,
    )
    findings = [
        finding
        for finding in json.loads(completed.stdout)["findings"]
        if finding["severity"] in LINT_FAULT_SEVERITIES
        and (media_are_present or not finding["code"].endswith(ABSENT_MEDIA_CODE_SUFFIX))
    ]
    if findings:
        raise LintFailed("\n".join(f"{finding['file']}: {finding['code']}: {finding['message']}" for finding in findings))
