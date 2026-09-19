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

A code diff larger than one Claude run reads is judged in parts
(``split_diff``), each in a run of its own and several runs at once, and every
gap records the part that shows the change it covers, so the updater hands
Claude that same part. No part of the diff is ever dropped.

A judge reads one slice of the diff, and a slice can mislead: a comment that
disagrees with the code it sits on, or a caller shown without the callee that
decides the behaviour. So a blocking gap only counts once a second run, which
reads the code in the repository rather than a diff, confirms it
(``_confirm_blocking``). A gap it refutes is logged with the evidence and
dropped: it neither fails the check nor reaches the updater, which would
otherwise write the misreading into the docs.

The diff helpers (``get_code_diff``, ``get_docs_diff``, ``truncate_diff``)
are shared with the release-notes generator (``release_summary.py``), so the
notes are drafted from the same changes the docs check judged.

Both require ``claude`` and ``git`` on PATH. No special auth handling:
the invoking environment must already have claude authenticated.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import threading
from collections import Counter
from collections.abc import Callable, Sequence
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from functools import partial
from pathlib import Path, PurePosixPath
from typing import TypeVar

from .claude import run_claude
from .cli import ReleaseError, console, need_cmd, run_with_error_handling
from .repo import get_repo_root


# The directory the docs check owns end to end: the updater edits it, and the
# release commits it onto a branch of its own when it finds it stale.
DOCS_DIR = "docs"

# Paths excluded from the code diff fed to claude: changes here don't imply
# doc updates are needed (and including docs/ would feed edits back as input).
_EXCLUDE_PREFIXES: tuple[str, ...] = (
    f"{DOCS_DIR}/",
    "target/",
    ".github/",
    "scripts/docs/",
    "scripts/functions/docs.py",
)

# Lock files, excluded from both diffs wherever they sit. They only record
# dependency versions, which neither the docs nor the release notes report,
# and a large bump would spend the diff budget before any code is reached.
_LOCK_FILE_NAMES: frozenset[str] = frozenset(
    {"Cargo.lock", "uv.lock", "pixi.lock", "package-lock.json"}
)

# peppy's own test suite, excluded from the code diff: both the docs check and
# the release notes are told to ignore it, so reading it only spends Claude's
# time. A test lives in a `tests/` directory, a `tests.rs` module, or a
# `*_tests.rs` module. Anything under a `templates/` directory is kept even
# when it matches, since templates are scaffolded or generated into users'
# projects (the `tests/` of a new node, for one).
_TEST_DIR_NAME = "tests"
_TEST_MODULE_NAME = "tests.rs"
_TEST_MODULE_SUFFIX = "_tests.rs"
_TEMPLATES_DIR_NAME = "templates"

# The user documentation: the pages the docs site renders, including the
# snippets they embed. Release notes under `docs/src/content/releases/` and the
# site's own configuration are not part of it.
USER_DOCS_PREFIX = "docs/src/content/docs/"

# The most diff one Claude run reads, in characters. The docs check judges a
# larger code diff in parts of at most this size (`split_diff`); the release
# notes cut their diffs to it (`truncate_diff`).
_MAX_DIFF_CHARS = 400_000

# How many Claude runs the docs check has going at once, judging parts of a
# code diff or confirming gaps. The runs only read and none depends on another;
# four keep a twenty-part diff to a few rounds without bursting past the
# account's rate limits.
_CHECK_CONCURRENCY = 4

# The `diff --git` line opening each file's section of a unified diff; the
# group is the file's path on the new side, quoted by git when it holds
# unusual characters.
_DIFF_HEADER = re.compile(r'^diff --git "?a/.*? "?b/(.*?)"?$', re.MULTILINE)

# The docs never use an em-dash (U+2014), and Claude writes one by habit. Every
# text of Claude's that reaches a docs pull request is held to its absence: the
# gap descriptions (quoted in the pull request body) through the check schema,
# and the updater's edits by `_reword_added_em_dashes`.
EM_DASH = "\u2014"
_NO_EM_DASH_PATTERN = f"^[^{EM_DASH}]*$"

SEVERITY_BLOCKING = "blocking"
SEVERITY_MINOR = "minor"

STATUS_IMPLEMENTED = "implemented"
STATUS_ALREADY_COVERED = "already_covered"

VERDICT_CONFIRMED = "confirmed"
VERDICT_REFUTED = "refuted"

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
                    "change": {"type": "string", "pattern": _NO_EM_DASH_PATTERN},
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

# Schema for the second opinion on one blocking gap. The evidence is logged
# with a dropped gap, so a wrong refutation can be argued with.
_CONFIRM_SCHEMA: dict = {
    "type": "object",
    "properties": {
        "verdict": {"enum": [VERDICT_CONFIRMED, VERDICT_REFUTED]},
        "evidence": {"type": "string"},
    },
    "required": ["verdict", "evidence"],
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

# Schema for the em-dash rewording report. The edits are the result and are
# verified on disk; the summary only gives the run a structured answer.
_REWORD_SCHEMA: dict = {
    "type": "object",
    "properties": {"summary": {"type": "string"}},
    "required": ["summary"],
    "additionalProperties": False,
}


@dataclass(frozen=True)
class RequiredChange:
    file: str
    change: str
    severity: str
    # 1-based number of the code diff part (`split_diff`) showing the change
    # this gap covers; the updater hands Claude that part when closing it.
    diff_part: int = 1


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
class GapReview:
    """The second opinion on one blocking gap, from the code in the repository."""

    confirmed: bool
    evidence: str


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


@dataclass(frozen=True)
class DiffPart:
    """A slice of a code diff small enough for one Claude run to read whole."""

    text: str
    paths: tuple[str, ...]


@dataclass(frozen=True)
class EmDashLine:
    """A line under ``docs/`` holding an em-dash, with its 1-based number."""

    file: str
    line: int
    text: str


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


def _is_lock_file(path: str) -> bool:
    return PurePosixPath(path).name in _LOCK_FILE_NAMES


def _is_own_test_path(path: str) -> bool:
    """True for a file of peppy's own test suite (see `_TEST_DIR_NAME`)."""
    *directories, name = PurePosixPath(path).parts
    if _TEMPLATES_DIR_NAME in directories:
        return False
    return (
        _TEST_DIR_NAME in directories
        or name == _TEST_MODULE_NAME
        or name.endswith(_TEST_MODULE_SUFFIX)
    )


def _is_code_path(path: str) -> bool:
    if _is_lock_file(path) or _is_own_test_path(path):
        return False
    return not any(path.startswith(p) for p in _EXCLUDE_PREFIXES)


def _is_user_docs_path(path: str) -> bool:
    return path.startswith(USER_DOCS_PREFIX) and not _is_lock_file(path)


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
    if len(diff) <= _MAX_DIFF_CHARS:
        return diff
    return (
        diff[:_MAX_DIFF_CHARS]
        + f"\n\n[diff truncated: original size {len(diff)} bytes]"
    )


def _file_sections(diff: str) -> list[tuple[str, str]]:
    """Split a unified diff into one (path, section) per file, in diff order."""
    headers = list(_DIFF_HEADER.finditer(diff))
    if diff and (not headers or headers[0].start() != 0):
        raise ReleaseError(
            "the code diff does not open with a 'diff --git' header, so it "
            "cannot be split into files"
        )
    sections: list[tuple[str, str]] = []
    for index, header in enumerate(headers):
        end = headers[index + 1].start() if index + 1 < len(headers) else len(diff)
        sections.append((header.group(1), diff[header.start() : end]))
    return sections


def _cut_section(section: str, cap: int) -> list[str]:
    """Cut one file's diff section into pieces of at most *cap* characters.

    Every piece starts with the section's header (the lines before its first
    hunk), so its lines stay attributed to the file. The cut falls between
    lines, or within a line that is past the cap on its own.
    """
    lines = section.splitlines(keepends=True)
    body_start = next(
        (i for i, line in enumerate(lines) if line.startswith("@@")), len(lines)
    )
    header = "".join(lines[:body_start])
    budget = cap - len(header)
    pieces: list[str] = []
    run: list[str] = []
    size = 0
    for line in lines[body_start:]:
        if size and size + len(line) > budget:
            pieces.append(header + "".join(run))
            run, size = [], 0
        if len(line) <= budget:
            run.append(line)
            size += len(line)
            continue
        pieces.extend(
            header + line[start : start + budget]
            for start in range(0, len(line), budget)
        )
    if run:
        pieces.append(header + "".join(run))
    return pieces


def _part_of(sections: list[tuple[str, str]]) -> DiffPart:
    return DiffPart(
        text="".join(section for _, section in sections),
        paths=tuple(path for path, _ in sections),
    )


def split_diff(diff: str, cap: int) -> tuple[DiffPart, ...]:
    """Cut *diff* into parts of at most *cap* characters, dropping none of it.

    Files stay whole and keep the diff's order, packed as many to a part as
    fit. A file whose own section is past the cap is cut into pieces of its
    own (`_cut_section`), each a part by itself.
    """
    parts: list[DiffPart] = []
    pending: list[tuple[str, str]] = []
    size = 0
    for path, section in _file_sections(diff):
        if size and size + len(section) > cap:
            parts.append(_part_of(pending))
            pending, size = [], 0
        if len(section) <= cap:
            pending.append((path, section))
            size += len(section)
            continue
        parts.extend(
            DiffPart(text=piece, paths=(path,))
            for piece in _cut_section(section, cap)
        )
    if pending:
        parts.append(_part_of(pending))
    return tuple(parts)


# Placed in a prompt when the diff is judged in parts: Claude sees one part
# per run and must not reason about, or report on, changes it cannot see.
_PART_SCOPE = """
The diff below is part {number} of {total} of the whole change set. The other
parts hold other files' changes and are handled in runs of their own, so act
on what this part shows only.
"""


def _part_name(number: int, total: int) -> str:
    """How the log names a part: 'the diff' when it is the whole of it."""
    if total == 1:
        return "the diff"
    return f"part {number} of {total} of the diff"


def _part_scope(number: int, total: int) -> str:
    """The prompt paragraph placing a part in the whole; empty for a whole diff."""
    if total == 1:
        return ""
    return _PART_SCOPE.format(number=number, total=total)


_CHECK_PROMPT = """\
You are judging whether the documentation in `docs/src/content/docs/` covers a
set of code changes. The project is "peppy": an Astro Starlight documentation
site paired with Rust crates under `crates/`.
{scope}
Your task:
1. Use Read/Grep/Glob to explore `docs/src/content/docs/` and compare it
   against the diff below. Judge only what this diff changes; this is not a
   general documentation audit.
2. Report every documentation gap the diff creates as an entry in
   `required_changes` (the doc file path, a one-sentence description of the
   edit needed, and a severity). The description is quoted in a pull request
   and handed to the writer of the edit, so never use an em-dash (U+2014) in
   it: reword with a comma, a colon, parentheses, or a separate sentence.
   - "blocking": the docs now state something false, or a user-facing change
     in the diff (a CLI flag or subcommand, a `peppy.json5` schema key, a
     message format, a step in a guide or workflow) is entirely undocumented.
   - "minor": everything else: wording, clarity, style, restructuring,
     extra cross-references, nice-to-have examples, mentioning a feature in
     more places. Docs being improvable is not the same as docs being out of
     date. When unsure between the two severities, choose "minor".
3. Ignore purely internal changes: refactors, tests, private APIs, build/CI
   changes, log strings, dependency bumps.
4. A comment, a docstring or a test name in the diff is not evidence of what
   the code does, and the diff may not show the code that decides a behaviour.
   The repository you are in holds the code at the head of the diff: before
   reporting a "blocking" gap, Read the code that enforces the behaviour and
   report the gap only when that code agrees with it. When a comment and the
   code disagree, the code is right and the docs follow the code.

An empty `required_changes` means the docs fully cover the diff.

Changed paths:
{paths}

Unified diff:
{diff}
"""


_UPDATE_PROMPT = """\
You are updating the documentation in `docs/src/content/docs/` to close a
fixed list of gaps left by a set of code changes. The project is "peppy": an
Astro Starlight documentation site paired with Rust crates under `crates/`.
{scope}
Gaps to close (implement exactly these, nothing else):
{changes}

Your task:
1. For each listed gap, Read the named doc file (and any closely related
   pages) and make the smallest edit that closes it. The diff below and the
   code it touches in the repository you are in are the source of truth for
   the facts: never state behaviour they do not show, and when a comment and
   the code disagree, document what the code does.
2. If on inspection the docs already cover a listed gap, skip it and edit
   nothing for it.
3. Only touch files under `docs/src/content/docs/`. Do not reword,
   restructure, or "improve" prose that is still accurate, and do not modify
   `.astro` config, `package.json`, or anything outside `docs/`.
4. Never write an em-dash (U+2014): this documentation does not use one.
   Reword with a comma, a colon, parentheses, or a separate sentence, and
   never replace it with another dash (a hyphen, a double hyphen, or an
   en-dash).
5. Report one `results` entry per listed gap (its file, its change text,
   and a status of "implemented" or "already_covered"), plus a short overall
   `summary`.

Changed paths:
{paths}

Unified diff:
{diff}
"""


_CONFIRM_PROMPT = """\
A reviewer who read one slice of a code diff reported the documentation gap
below, and a confirmed gap stops a release. The project is "peppy": an Astro
Starlight documentation site under `docs/src/content/docs/` paired with the
Rust and Python code around it. The repository you are in holds the code being
released and its documentation. Establish whether the gap is real.

Reported gap:
- doc file: `{file}`
- claim: {change}

The slice the reviewer read changed these files:
{paths}

Your task:
1. Read the doc file and check it says, or lacks, what the claim says it does.
2. Find the code that decides the behaviour the claim is about, starting from
   the files above and following the calls down to the code that enforces it.
   A comment, a docstring, a test name or a log string is not evidence of
   behaviour: when one disagrees with the code, the code is right.
3. Answer "confirmed" only when both hold: the code behaves as the claim
   says, and the docs state otherwise or leave a user-facing change entirely
   undocumented. Answer "refuted" in every other case: the code behaves
   differently, the docs already cover it, the docs are merely improvable, the
   evidence is mixed, or you cannot find the code that decides it.
4. Give in `evidence` the code (file and lines) and the doc passage that
   decide your answer, in a sentence or two.
"""


_REWORD_PROMPT = """\
Documentation edits under `docs/` added the lines below, and each holds an
em-dash (U+2014), which this documentation never uses. Reword every listed
line so it holds none: use a comma, a colon, parentheses, or a separate
sentence, and never replace it with another dash (a hyphen, a double hyphen, or
an en-dash). Keep each line's meaning, and edit nothing but these lines.

{lines}
"""


def _parse_check_response(payload: dict, diff_part: int = 1) -> CheckResult:
    """Validate the check verdict object into a CheckResult.

    The CLI already validated the payload against ``_CHECK_SCHEMA``, but that
    enforcement lives in an unpinned external tool; re-checking here keeps a
    drifted CLI surfacing as a ReleaseError instead of a stray KeyError.
    Every change is stamped with *diff_part*, the part of the diff it was
    judged on.
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
        if EM_DASH in change:
            raise ReleaseError(
                f"claude verdict change entry holds an em-dash: {item!r}"
            )
        changes.append(
            RequiredChange(
                file=file, change=change, severity=severity, diff_part=diff_part
            )
        )
    return CheckResult(changes=tuple(changes))


def _parse_confirm_response(payload: dict) -> GapReview:
    """Validate the second opinion on a gap into a GapReview."""
    verdict = payload.get("verdict")
    if verdict not in (VERDICT_CONFIRMED, VERDICT_REFUTED):
        raise ReleaseError(f"claude gap review has unknown verdict: {payload!r}")
    evidence = payload.get("evidence")
    if not isinstance(evidence, str):
        raise ReleaseError(
            f"claude gap review missing string 'evidence': {payload!r}"
        )
    return GapReview(confirmed=verdict == VERDICT_CONFIRMED, evidence=evidence)


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


def _em_dash_lines(repo_root: Path) -> tuple[EmDashLine, ...]:
    """Every line under ``docs/`` holding an em-dash, committed or not.

    Untracked files count (the updater may write a new page); ignored and
    binary files do not.
    """
    result = subprocess.run(
        [
            "git",
            "grep",
            "-z",
            "-n",
            "-I",
            "--untracked",
            "-F",
            "-e",
            EM_DASH,
            "--",
            DOCS_DIR,
        ],
        cwd=repo_root,
        capture_output=True,
        encoding="utf-8",
        errors="replace",
    )
    # git grep exits 1 when nothing matches.
    if result.returncode == 1 and not result.stderr.strip():
        return ()
    if result.returncode != 0:
        raise ReleaseError(
            f"git grep for em-dashes under '{DOCS_DIR}/' failed: "
            f"{result.stderr.strip()}"
        )
    lines: list[EmDashLine] = []
    for record in result.stdout.splitlines():
        file, line, text = record.split("\0", 2)
        lines.append(EmDashLine(file=file, line=int(line), text=text.strip()))
    return tuple(lines)


def _added_em_dash_lines(
    before: tuple[EmDashLine, ...], repo_root: Path
) -> tuple[EmDashLine, ...]:
    """The em-dash lines under ``docs/`` that were not there *before*.

    Lines are matched by file and text, not number, so an edit that only moves
    an existing line never counts it as added.
    """
    existing = Counter((line.file, line.text) for line in before)
    added: list[EmDashLine] = []
    for line in _em_dash_lines(repo_root):
        key = (line.file, line.text)
        if existing[key]:
            existing[key] -= 1
            continue
        added.append(line)
    return tuple(added)


def _render_em_dash_lines(lines: tuple[EmDashLine, ...]) -> str:
    return "\n".join(
        f"- `{line.file}` line {line.line}: {line.text}" for line in lines
    )


def _reword_added_em_dashes(
    before: tuple[EmDashLine, ...], repo_root: Path
) -> None:
    """Remove the em-dashes an edit added under ``docs/``, or stop.

    The update prompt already forbids them; this is what guarantees none
    reaches a pull request. Claude gets one pass at rewording the lines, and
    any it leaves stop the release with the lines named.
    """
    added = _added_em_dash_lines(before, repo_root)
    if not added:
        return
    console.print(
        f"[yellow]The docs update added {len(added)} line(s) holding an "
        f"em-dash; asking Claude to reword them...[/yellow]"
    )
    run_claude(
        _REWORD_PROMPT.format(lines=_render_em_dash_lines(added)),
        allowed_tools="Read Edit",
        permission_mode="acceptEdits",
        cwd=repo_root,
        json_schema=_REWORD_SCHEMA,
        activity="rewording the em-dash lines",
        tools="Read Edit",
    )
    remaining = _added_em_dash_lines(before, repo_root)
    if remaining:
        raise ReleaseError(
            f"the docs update still adds an em-dash after rewording, and the "
            f"docs never use one:\n{_render_em_dash_lines(remaining)}\n"
            f"The edits are left under '{DOCS_DIR}/'; reword those lines by hand."
        )


def _judge_part(
    part: DiffPart, number: int, total: int, repo_root: Path
) -> CheckResult:
    """Have one Claude run judge *part*, the *number*-th of *total* parts."""
    name = _part_name(number, total)
    console.print(
        f"Asking Claude to judge {name} ({len(part.paths)} path(s), "
        f"{len(part.text) // 1024} KB)..."
    )
    prompt = _CHECK_PROMPT.format(
        scope=_part_scope(number, total),
        paths="\n".join(part.paths),
        diff=part.text,
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
        activity=f"judging {name}",
        tools="Read Grep Glob",
    )
    result = _parse_check_response(payload, diff_part=number)
    console.print(f"[dim]Judged {name}: {len(result.changes)} gap(s).[/dim]")
    return result


_Result = TypeVar("_Result")


def _run_at_once(jobs: Sequence[Callable[[], _Result]]) -> list[_Result]:
    """Run *jobs* `_CHECK_CONCURRENCY` at a time; the results in job order.

    A failing job stops every job not yet started. The jobs already under way
    finish, then the failure of the first failing job is raised.
    """
    failed = threading.Event()

    def run(job: Callable[[], _Result]) -> _Result | None:
        if failed.is_set():
            return None
        try:
            return job()
        except BaseException:
            # Set before the future completes, so a worker taking the next
            # job already sees it.
            failed.set()
            raise

    with ThreadPoolExecutor(max_workers=_CHECK_CONCURRENCY) as pool:
        futures = [pool.submit(run, job) for job in jobs]
    errors = [
        error for future in futures if (error := future.exception()) is not None
    ]
    if errors:
        raise errors[0]
    # No job failed, so none was skipped.
    return [
        result for future in futures if (result := future.result()) is not None
    ]


def _judge_parts(
    parts: tuple[DiffPart, ...], repo_root: Path
) -> list[CheckResult]:
    """Judge every part, several at once (`_run_at_once`); verdicts in part order."""
    return _run_at_once(
        [
            partial(_judge_part, part, number, len(parts), repo_root)
            for number, part in enumerate(parts, 1)
        ]
    )


def _review_gap(change: RequiredChange, part: DiffPart, repo_root: Path) -> GapReview:
    """Have one Claude run confirm or refute *change* against the repository.

    The run gets no diff: it is pointed at the files *part* changed and reads
    the code and the docs as they stand, so its answer does not inherit
    whatever misled the judge of that part.
    """
    console.print(f"Asking Claude to confirm the gap in {change.file}...")
    prompt = _CONFIRM_PROMPT.format(
        file=change.file,
        change=change.change,
        paths="\n".join(part.paths),
    )
    # Read-only for the same reason the judging runs are.
    payload = run_claude(
        prompt,
        allowed_tools="Read Grep Glob",
        permission_mode="bypassPermissions",
        cwd=repo_root,
        json_schema=_CONFIRM_SCHEMA,
        activity=f"confirming the gap in {change.file}",
        tools="Read Grep Glob",
    )
    return _parse_confirm_response(payload)


def _confirm_blocking(
    changes: tuple[RequiredChange, ...],
    parts: tuple[DiffPart, ...],
    repo_root: Path,
) -> tuple[RequiredChange, ...]:
    """Return *changes* without the blocking gaps a second run refutes.

    Every distinct blocking gap gets a run of its own (`_review_gap`), several
    at once. Minor suggestions pass through: they gate nothing, so a second
    opinion on them would only spend Claude's time.
    """
    blocking = tuple(
        dict.fromkeys(c for c in changes if c.severity == SEVERITY_BLOCKING)
    )
    if not blocking:
        return changes
    reviews = _run_at_once(
        [
            partial(_review_gap, change, parts[change.diff_part - 1], repo_root)
            for change in blocking
        ]
    )
    refuted: set[RequiredChange] = set()
    for change, review in zip(blocking, reviews, strict=True):
        if review.confirmed:
            console.print(f"[dim]Confirmed the gap in {change.file}.[/dim]")
            continue
        refuted.add(change)
        console.print(
            f"[yellow]Dropped a gap the code does not confirm:[/yellow]\n"
            f"  [bold]{change.file}[/bold]: {change.change}\n"
            f"  [dim]{review.evidence}[/dim]"
        )
    return tuple(change for change in changes if change not in refuted)


def check_docs(base: str, head: str) -> CheckResult:
    """Check whether ``docs/`` reflects code changes between base and head.

    The code diff is judged in parts of at most `_MAX_DIFF_CHARS`, one Claude
    run each and `_CHECK_CONCURRENCY` runs at a time, and the verdicts are
    merged in part order; every change names its part. A blocking gap is kept
    only when a second run confirms it against the code (`_confirm_blocking`).
    """
    repo_root = get_repo_root()
    diff, paths = get_code_diff(base, head, repo_root)
    if not paths:
        return CheckResult(changes=())
    parts = split_diff(diff, _MAX_DIFF_CHARS)
    console.print(
        f"[dim]{len(paths)} changed code path(s), {len(diff) // 1024} KB of "
        f"diff, judged in {len(parts)} part(s).[/dim]"
    )
    verdicts = _judge_parts(parts, repo_root)
    reported = tuple(change for verdict in verdicts for change in verdict.changes)
    return CheckResult(changes=_confirm_blocking(reported, parts, repo_root))


def update_docs(
    base: str, head: str, changes: tuple[RequiredChange, ...]
) -> UpdateResult:
    """Have claude close exactly *changes* in ``docs/`` for the base..head diff.

    The change list scopes the edits: the updater implements those gaps and
    nothing else, so the resulting diff is derived from the verdict rather
    than from a free-form re-audit of the docs. The diff is split into the
    same parts the check judged, and each part with gaps gets a run of its
    own, handed those gaps and that part. The edits add no em-dash.
    """
    if not changes:
        raise ReleaseError("update_docs called with no changes to implement")
    repo_root = get_repo_root()
    em_dashes_before = _em_dash_lines(repo_root)
    diff, _ = get_code_diff(base, head, repo_root)
    parts = split_diff(diff, _MAX_DIFF_CHARS)
    strays = [c for c in changes if not 1 <= c.diff_part <= len(parts)]
    if strays:
        raise ReleaseError(
            f"the diff {base}..{head} has {len(parts)} part(s), but these "
            f"changes name another: {strays!r}"
        )
    results: list[UpdateOutcome] = []
    summaries: list[str] = []
    for number, part in enumerate(parts, 1):
        part_changes = [c for c in changes if c.diff_part == number]
        if not part_changes:
            continue
        name = _part_name(number, len(parts))
        console.print(
            f"Asking Claude to close {len(part_changes)} gap(s) shown by {name}..."
        )
        prompt = _UPDATE_PROMPT.format(
            scope=_part_scope(number, len(parts)),
            changes="\n".join(f"- `{c.file}`: {c.change}" for c in part_changes),
            paths="\n".join(part.paths),
            diff=part.text,
        )
        payload = run_claude(
            prompt,
            allowed_tools="Read Edit Write Grep Glob",
            permission_mode="acceptEdits",
            cwd=repo_root,
            json_schema=_UPDATE_SCHEMA,
            activity=f"updating the docs for {name}",
            tools="Read Edit Write Grep Glob",
        )
        result = _parse_update_response(payload)
        results.extend(result.results)
        summaries.append(result.summary)
    _reword_added_em_dashes(em_dashes_before, repo_root)
    return UpdateResult(results=tuple(results), summary="\n".join(summaries))


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
    console.print("[red]docs are out of date, blocking changes:[/red]")
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
        console.print("[green]docs are up to date, nothing to update[/green]")
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
