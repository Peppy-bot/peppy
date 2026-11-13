"""Runs the HyperFrames command line of the project.

Each run keeps the caches of HyperFrames inside the project, sends no usage
data, and uses the Python and the FFmpeg of the pixi environment.
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
# The lint finding about the number of lines of a composition file. A compiled
# composition is the output of a template with the texts of the transcript in
# it, so its number of lines does not say how large the template is that a
# person reads and changes.
COMPILED_FILE_SIZE_CODE = "composition_file_too_large"


class LintFailed(Exception):
    """The lint of HyperFrames found a fault in the compiled project."""


class MissingProgram(Exception):
    """The pixi environment does not have a program that HyperFrames runs."""


def environment() -> dict[str, str]:
    """The environment of a run.

    HyperFrames finds a program by its name only in a folder outside the
    project, and the pixi environment is inside the project. Thus each program
    that HyperFrames runs has its path here. Without the path, HyperFrames
    runs the FFmpeg of the machine, which is not the pinned one.
    """
    return {
        **os.environ,
        "HYPERFRAMES_CACHE_DIR": str(paths.HYPERFRAMES_CACHE),
        "HYPERFRAMES_PYTHON": sys.executable,
        "HYPERFRAMES_FFMPEG_PATH": str(pinned_program("ffmpeg")),
        "HYPERFRAMES_FFPROBE_PATH": str(pinned_program("ffprobe")),
        "HYPERFRAMES_NO_TELEMETRY": "1",
        "HYPERFRAMES_NO_UPDATE_CHECK": "1",
        "DO_NOT_TRACK": "1",
    }


def pinned_program(name: str) -> Path:
    """The program `name` of the pixi environment, which is beside its Python."""
    program = Path(sys.executable).parent / name
    if not program.is_file():
        raise MissingProgram(f"the environment of {sys.executable} has no `{name}`: run the command with `pixi run`")
    return program


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
    the finding that a media file is absent is not a fault. The finding about
    the number of lines of a compiled composition is never a fault.
    """
    completed = subprocess.run(
        [str(EXECUTABLE), "lint", str(project), "--json"],
        cwd=paths.PROJECT_ROOT,
        env=environment(),
        capture_output=True,
        text=True,
    )
    faults = lint_faults(json.loads(completed.stdout)["findings"], media_are_present)
    if faults:
        raise LintFailed("\n".join(f"{fault['file']}: {fault['code']}: {fault['message']}" for fault in faults))


def lint_faults(findings: list[dict], media_are_present: bool) -> list[dict]:
    """The findings of a lint that are faults of the compositions."""
    return [
        finding
        for finding in findings
        if finding["severity"] in LINT_FAULT_SEVERITIES
        and finding["code"] != COMPILED_FILE_SIZE_CODE
        and (media_are_present or not finding["code"].endswith(ABSENT_MEDIA_CODE_SUFFIX))
    ]
