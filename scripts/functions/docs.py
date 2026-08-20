"""Documentation freshness check and auto-update via the claude CLI.

Two entry points:

- ``check_main``: diff two git refs, ask claude whether ``docs/`` has any
  blocking gap for the changes, and exit non-zero with the list if so.
- ``update_main``: same diff, then let claude close exactly the blocking
  gaps the check found by editing files under ``docs/`` in place.

A gap is *blocking* only when the docs now state something false or a
user-facing change is entirely undocumented. Wording, clarity, and other
nice-to-haves are reported as *minor* and never fail the check or feed the
updater: tooling must not generate wording churn, and a release must not
hinge on how the model would phrase a sentence today.

The diff helpers (``get_code_diff``, ``get_docs_diff``, ``truncate_diff``)
are shared with the release-notes generator (``release_summary.py``), so the
notes are drafted from the same changes the docs check judged.

Both require ``claude`` and ``git`` on PATH. No special auth handling —
the invoking environment must already have claude authenticated.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

from .claude import run_claude
from .cli import ReleaseError, console, need_cmd, run_with_error_handling
from .repo import get_repo_root


# Paths excluded from the code diff fed to claude — changes here don't imply
# doc updates are needed (and including docs/ would feed edits back as input).
_EXCLUDE_PREFIXES: tuple[str, ...] = (
    "docs/",
    "target/",
    ".github/",
    "scripts/docs/",
    "scripts/functions/docs.py",
    "scripts/tests/test_docs.py",
)

# Files excluded from the code diff outright. The lock file only records
# dependency versions, which neither the docs nor the release notes report, and
# it sorts first in `git diff` output, so a large bump would spend the diff
# budget before any code is reached.
_EXCLUDE_PATHS: tuple[str, ...] = ("Cargo.lock",)

# The user documentation: the pages the docs site renders, including the
# snippets they embed. Release notes under `docs/src/content/releases/` and the
# site's own configuration are not part of it.
USER_DOCS_PREFIX = "docs/src/content/docs/"

_MAX_DIFF_BYTES = 400_000

SEVERITY_BLOCKING = "blocking"
SEVERITY_MINOR = "minor"

STATUS_IMPLEMENTED = "implemented"
STATUS_ALREADY_COVERED = "already_covered"

# Schema for the check verdict, enforced CLI-side via --json-schema. There is
# deliberately no up-to-date boolean: pass/fail is derived from the list, so
# the model cannot contradict itself, and the severity split gives it an
# outlet for borderline observations without inflating them into blockers.
_CHECK_SCHEMA: dict = {
    "type": "object",
    "properties": {
        "required_changes": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "file": {"type": "string"},
                    "change": {"type": "string"},
                    "severity": {"enum": [SEVERITY_BLOCKING, SEVERITY_MINOR]},
                },
                "required": ["file", "change", "severity"],
                "additionalProperties": False,
            },
        },
    },
    "required": ["required_changes"],
    "additionalProperties": False,
}

# Schema for the update report: one entry per requested change, so the caller
# can tell "nothing to do, docs already cover it" apart from a silent no-op.
_UPDATE_SCHEMA: dict = {
    "type": "object",
    "properties": {
        "results": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "file": {"type": "string"},
                    "change": {"type": "string"},
                    "status": {
                        "enum": [STATUS_IMPLEMENTED, STATUS_ALREADY_COVERED]
                    },
                },
                "required": ["file", "change", "status"],
                "additionalProperties": False,
            },
        },
        "summary": {"type": "string"},
    },
    "required": ["results", "summary"],
    "additionalProperties": False,
}


@dataclass(frozen=True)
class RequiredChange:
    file: str
    change: str
    severity: str


@dataclass(frozen=True)
class CheckResult:
    changes: tuple[RequiredChange, ...]

    @property
    def blocking(self) -> tuple[RequiredChange, ...]:
        return tuple(c for c in self.changes if c.severity == SEVERITY_BLOCKING)

    @property
    def minor(self) -> tuple[RequiredChange, ...]:
        return tuple(c for c in self.changes if c.severity == SEVERITY_MINOR)


@dataclass(frozen=True)
class UpdateOutcome:
    file: str
    change: str
    status: str


@dataclass(frozen=True)
class UpdateResult:
    results: tuple[UpdateOutcome, ...]
    summary: str

    @property
    def all_already_covered(self) -> bool:
        """True when the updater accounted for every gap as already documented.

        An empty report does not count: the updater must name what it checked,
        otherwise a malfunctioning run would read as a clean verdict.
        """
        return bool(self.results) and all(
            r.status == STATUS_ALREADY_COVERED for r in self.results
        )


def _run_git(args: list[str], cwd: Path) -> str:
    result = subprocess.run(
        ["git", *args],
        cwd=cwd,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(
            f"git {' '.join(args)} failed: {result.stderr.strip()}"
        )
    return result.stdout


def _is_code_path(path: str) -> bool:
    if path in _EXCLUDE_PATHS:
        return False
    return not any(path.startswith(p) for p in _EXCLUDE_PREFIXES)


def _is_user_docs_path(path: str) -> bool:
    return path.startswith(USER_DOCS_PREFIX)


def _diff_matching(
    base: str, head: str, repo_root: Path, keep: Callable[[str], bool]
) -> tuple[str, list[str]]:
    """Return (diff_text, changed_paths) between refs for the paths *keep* accepts.

    The changed paths are listed first so the diff itself is only read for the
    paths that matter; when none match, git is not asked for a diff at all.
    """
    names_raw = _run_git(
        ["diff", "--name-only", f"{base}..{head}"],
        cwd=repo_root,
    )
    changed = [line for line in names_raw.splitlines() if line.strip()]
    kept = [p for p in changed if keep(p)]
    if not kept:
        return "", []
    diff = _run_git(
        ["diff", f"{base}..{head}", "--", *kept],
        cwd=repo_root,
    )
    return diff, kept


def get_code_diff(base: str, head: str, repo_root: Path) -> tuple[str, list[str]]:
    """Return (diff_text, changed_paths) for code-only changes between refs."""
    return _diff_matching(base, head, repo_root, _is_code_path)


def get_docs_diff(base: str, head: str, repo_root: Path) -> tuple[str, list[str]]:
    """Return (diff_text, changed_paths) for user documentation changes between refs."""
    return _diff_matching(base, head, repo_root, _is_user_docs_path)


def truncate_diff(diff: str) -> str:
    if len(diff) <= _MAX_DIFF_BYTES:
        return diff
    return (
        diff[:_MAX_DIFF_BYTES]
        + f"\n\n[diff truncated — original size {len(diff)} bytes]"
    )


_CHECK_PROMPT = """\
You are judging whether the documentation in `docs/src/content/docs/` covers a
set of code changes. The project is "peppy" — an Astro Starlight documentation
site paired with Rust crates under `crates/`.

Your task:
1. Use Read/Grep/Glob to explore `docs/src/content/docs/` and compare it
   against the diff below. Judge only what this diff changes — this is not a
   general documentation audit.
2. Report every documentation gap the diff creates as an entry in
   `required_changes` (the doc file path, a one-sentence description of the
   edit needed, and a severity):
   - "blocking": the docs now state something false, or a user-facing change
     in the diff (a CLI flag or subcommand, a `peppy.json5` schema key, a
     message format, a step in a guide or workflow) is entirely undocumented.
   - "minor": everything else — wording, clarity, style, restructuring,
     extra cross-references, nice-to-have examples, mentioning a feature in
     more places. Docs being improvable is not the same as docs being out of
     date. When unsure between the two severities, choose "minor".
3. Ignore purely internal changes: refactors, tests, private APIs, build/CI
   changes, log strings, dependency bumps.

An empty `required_changes` means the docs fully cover the diff.

Changed paths:
{paths}

Unified diff:
{diff}
"""


_UPDATE_PROMPT = """\
You are updating the documentation in `docs/src/content/docs/` to close a
fixed list of gaps left by a set of code changes. The project is "peppy" — an
Astro Starlight documentation site paired with Rust crates under `crates/`.

Gaps to close — implement exactly these, nothing else:
{changes}

Your task:
1. For each listed gap, Read the named doc file (and any closely related
   pages) and make the smallest edit that closes it. The diff below is the
   source of truth for the facts — never state behaviour it does not show.
2. If on inspection the docs already cover a listed gap, skip it and edit
   nothing for it.
3. Only touch files under `docs/src/content/docs/`. Do not reword,
   restructure, or "improve" prose that is still accurate, and do not modify
   `.astro` config, `package.json`, or anything outside `docs/`.
4. Report one `results` entry per listed gap — its file, its change text,
   and a status of "implemented" or "already_covered" — plus a short overall
   `summary`.

Changed paths:
{paths}

Unified diff:
{diff}
"""


def _parse_check_response(payload: dict) -> CheckResult:
    """Validate the check verdict object into a CheckResult.

    The CLI already validated the payload against ``_CHECK_SCHEMA``, but that
    enforcement lives in an unpinned external tool; re-checking here keeps a
    drifted CLI surfacing as a ReleaseError instead of a stray KeyError.
    """
    raw_changes = payload.get("required_changes")
    if not isinstance(raw_changes, list):
        raise ReleaseError(
            f"claude verdict 'required_changes' must be a list: {payload!r}"
        )
    changes: list[RequiredChange] = []
    for item in raw_changes:
        if not isinstance(item, dict):
            raise ReleaseError(
                f"claude verdict change entry must be an object: {item!r}"
            )
        file = item.get("file")
        change = item.get("change")
        severity = item.get("severity")
        if not isinstance(file, str) or not isinstance(change, str):
            raise ReleaseError(
                f"claude verdict change entry missing file/change: {item!r}"
            )
        if severity not in (SEVERITY_BLOCKING, SEVERITY_MINOR):
            raise ReleaseError(
                f"claude verdict change entry has unknown severity: {item!r}"
            )
        changes.append(
            RequiredChange(file=file, change=change, severity=severity)
        )
    return CheckResult(changes=tuple(changes))


def _parse_update_response(payload: dict) -> UpdateResult:
    """Validate the update report object into an UpdateResult."""
    raw_results = payload.get("results")
    if not isinstance(raw_results, list):
        raise ReleaseError(
            f"claude update report 'results' must be a list: {payload!r}"
        )
    summary = payload.get("summary")
    if not isinstance(summary, str):
        raise ReleaseError(
            f"claude update report missing string 'summary': {payload!r}"
        )
    results: list[UpdateOutcome] = []
    for item in raw_results:
        if not isinstance(item, dict):
            raise ReleaseError(
                f"claude update report entry must be an object: {item!r}"
            )
        file = item.get("file")
        change = item.get("change")
        status = item.get("status")
        if not isinstance(file, str) or not isinstance(change, str):
            raise ReleaseError(
                f"claude update report entry missing file/change: {item!r}"
            )
        if status not in (STATUS_IMPLEMENTED, STATUS_ALREADY_COVERED):
            raise ReleaseError(
                f"claude update report entry has unknown status: {item!r}"
            )
        results.append(UpdateOutcome(file=file, change=change, status=status))
    return UpdateResult(results=tuple(results), summary=summary)


def check_docs(base: str, head: str) -> CheckResult:
    """Check whether ``docs/`` reflects code changes between base and head."""
    repo_root = get_repo_root()
    diff, paths = get_code_diff(base, head, repo_root)
    if not paths:
        return CheckResult(changes=())
    prompt = _CHECK_PROMPT.format(
        paths="\n".join(paths),
        diff=truncate_diff(diff),
    )
    # tools (not just allowed_tools) is restricted: under bypassPermissions
    # the allowlist approves rather than limits, and the check must stay
    # read-only.
    payload = run_claude(
        prompt,
        allowed_tools="Read Grep Glob",
        permission_mode="bypassPermissions",
        cwd=repo_root,
        json_schema=_CHECK_SCHEMA,
        tools="Read Grep Glob",
    )
    return _parse_check_response(payload)


def update_docs(
    base: str, head: str, changes: tuple[RequiredChange, ...]
) -> UpdateResult:
    """Have claude close exactly *changes* in ``docs/`` for the base..head diff.

    The change list scopes the edits: the updater implements those gaps and
    nothing else, so the resulting diff is derived from the verdict rather
    than from a free-form re-audit of the docs.
    """
    if not changes:
        raise ReleaseError("update_docs called with no changes to implement")
    repo_root = get_repo_root()
    diff, paths = get_code_diff(base, head, repo_root)
    prompt = _UPDATE_PROMPT.format(
        changes="\n".join(f"- `{c.file}`: {c.change}" for c in changes),
        paths="\n".join(paths),
        diff=truncate_diff(diff),
    )
    payload = run_claude(
        prompt,
        allowed_tools="Read Edit Write Grep Glob",
        permission_mode="acceptEdits",
        cwd=repo_root,
        json_schema=_UPDATE_SCHEMA,
        tools="Read Edit Write Grep Glob",
    )
    return _parse_update_response(payload)


def print_minor_changes(minor: tuple[RequiredChange, ...]) -> None:
    """Print minor doc suggestions as information; they never gate anything."""
    if not minor:
        return
    console.print(
        f"[dim]{len(minor)} minor doc suggestion(s) noted "
        f"(never block anything):[/dim]"
    )
    for change in minor:
        console.print(f"  [dim]{change.file}: {change.change}[/dim]")


def _parse_args(prog: str) -> argparse.Namespace:
    parser = argparse.ArgumentParser(prog=prog)
    parser.add_argument("base", help="base git ref (e.g. origin/main)")
    parser.add_argument("head", help="head git ref (e.g. HEAD)")
    return parser.parse_args()


def _run_check() -> None:
    need_cmd("git")
    need_cmd("claude")
    args = _parse_args("is-doc-up-to-date")
    result = check_docs(args.base, args.head)
    print_minor_changes(result.minor)
    if not result.blocking:
        console.print("[green]docs are up to date[/green]")
        return
    console.print("[red]docs are out of date — blocking changes:[/red]")
    for change in result.blocking:
        console.print(f"  [bold]{change.file}[/bold]: {change.change}")
    sys.exit(1)


def _run_update() -> None:
    need_cmd("git")
    need_cmd("claude")
    args = _parse_args("update-docs")
    check = check_docs(args.base, args.head)
    print_minor_changes(check.minor)
    if not check.blocking:
        console.print("[green]docs are up to date — nothing to update[/green]")
        return
    update = update_docs(args.base, args.head, check.blocking)
    for outcome in update.results:
        console.print(
            f"  [bold]{outcome.file}[/bold] ({outcome.status}): {outcome.change}"
        )
    console.print(update.summary)


def check_main() -> None:
    run_with_error_handling(_run_check)


def update_main() -> None:
    run_with_error_handling(_run_update)
