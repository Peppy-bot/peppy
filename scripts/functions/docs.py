"""Documentation freshness check and auto-update via the claude CLI.

Two entry points:

- ``check_main``: diff two git refs, ask claude whether ``docs/`` is up to
  date, and exit non-zero with a list of required edits if not.
- ``update_main``: same diff, but let claude edit files under ``docs/``
  in place.

Both require ``claude`` and ``git`` on PATH. No special auth handling —
the invoking environment must already have claude authenticated.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

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

_MAX_DIFF_BYTES = 400_000


@dataclass(frozen=True)
class RequiredChange:
    file: str
    change: str


@dataclass(frozen=True)
class CheckResult:
    up_to_date: bool
    required_changes: tuple[RequiredChange, ...]


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
    return not any(path.startswith(p) for p in _EXCLUDE_PREFIXES)


def get_code_diff(base: str, head: str, repo_root: Path) -> tuple[str, list[str]]:
    """Return (diff_text, changed_paths) for code-only changes between refs."""
    names_raw = _run_git(
        ["diff", "--name-only", f"{base}..{head}"],
        cwd=repo_root,
    )
    changed = [line for line in names_raw.splitlines() if line.strip()]
    code_paths = [p for p in changed if _is_code_path(p)]
    if not code_paths:
        return "", []
    diff = _run_git(
        ["diff", f"{base}..{head}", "--", *code_paths],
        cwd=repo_root,
    )
    return diff, code_paths


def _truncate_diff(diff: str) -> str:
    if len(diff) <= _MAX_DIFF_BYTES:
        return diff
    return (
        diff[:_MAX_DIFF_BYTES]
        + f"\n\n[diff truncated — original size {len(diff)} bytes]"
    )


_CHECK_PROMPT = """\
You are validating whether the documentation in `docs/src/content/docs/` is up
to date with a set of code changes. The project is "peppy" — an Astro Starlight
documentation site paired with Rust crates under `crates/`.

Your task:
1. Use Read/Grep/Glob to explore `docs/src/content/docs/` and identify any
   user-facing changes in the diff below that require doc updates — CLI
   flags and subcommands, `peppy.json5` schema, message formats, guide
   steps, concepts, container workflows, etc.
2. Ignore purely internal changes: refactors, tests, private APIs, build/CI
   changes, log strings, dependency bumps.
3. Return a JSON verdict as your final assistant message. No prose, no
   code fence, no explanation — exactly this shape:

{{"up_to_date": <bool>, "required_changes": [{{"file": "<doc path>", "change": "<one-sentence description>"}}]}}

If `up_to_date` is true, `required_changes` must be `[]`.

Changed paths:
{paths}

Unified diff:
{diff}
"""


_UPDATE_PROMPT = """\
You are updating the documentation in `docs/src/content/docs/` to reflect a
set of code changes. The project is "peppy" — an Astro Starlight
documentation site paired with Rust crates under `crates/`.

Your task:
1. Use Read/Grep/Glob to explore `docs/src/content/docs/` and identify any
   user-facing changes in the diff below that require doc updates — CLI
   flags and subcommands, `peppy.json5` schema, message formats, guide
   steps, concepts, container workflows, etc.
2. Edit docs files directly using Edit/Write. Only touch files under
   `docs/src/content/docs/`. Do not modify `.astro` config, `package.json`,
   or anything outside `docs/`.
3. Keep edits minimal and focused — do not rewrite prose that is still
   accurate, and do not fabricate features.
4. After editing, summarize the files you changed in a few bullet points.

Changed paths:
{paths}

Unified diff:
{diff}
"""


def _run_claude(
    prompt: str,
    *,
    allowed_tools: str,
    permission_mode: str,
    cwd: Path,
) -> str:
    """Run ``claude -p`` and return the final assistant text."""
    cmd = [
        "claude",
        "-p",
        prompt,
        "--output-format",
        "json",
        "--permission-mode",
        permission_mode,
        "--allowed-tools",
        allowed_tools,
    ]
    result = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)
    if result.returncode != 0:
        raise ReleaseError(
            f"claude CLI failed (exit {result.returncode}): "
            f"{result.stderr.strip() or result.stdout.strip()}"
        )
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as e:
        raise ReleaseError(
            f"claude did not return valid JSON ({e}); stdout={result.stdout[:500]!r}"
        )
    final_text = payload.get("result")
    if not isinstance(final_text, str):
        raise ReleaseError(
            f"claude response missing 'result' string: {payload!r}"
        )
    return final_text


def _strip_code_fence(text: str) -> str:
    text = text.strip()
    if not text.startswith("```"):
        return text
    lines = text.splitlines()[1:]
    if lines and lines[-1].strip().startswith("```"):
        lines = lines[:-1]
    return "\n".join(lines).strip()


def _parse_check_response(text: str) -> CheckResult:
    stripped = _strip_code_fence(text)
    try:
        payload = json.loads(stripped)
    except json.JSONDecodeError as e:
        raise ReleaseError(
            f"claude verdict was not valid JSON ({e}); text={text[:500]!r}"
        )
    if not isinstance(payload, dict):
        raise ReleaseError(f"claude verdict must be an object: {payload!r}")
    up_to_date = payload.get("up_to_date")
    if not isinstance(up_to_date, bool):
        raise ReleaseError(
            f"claude verdict missing boolean 'up_to_date': {payload!r}"
        )
    raw_changes = payload.get("required_changes", [])
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
        if not isinstance(file, str) or not isinstance(change, str):
            raise ReleaseError(
                f"claude verdict change entry missing file/change: {item!r}"
            )
        changes.append(RequiredChange(file=file, change=change))
    return CheckResult(up_to_date=up_to_date, required_changes=tuple(changes))


def check_docs(base: str, head: str) -> CheckResult:
    """Check whether ``docs/`` reflects code changes between base and head."""
    repo_root = get_repo_root()
    diff, paths = get_code_diff(base, head, repo_root)
    if not paths:
        return CheckResult(up_to_date=True, required_changes=())
    prompt = _CHECK_PROMPT.format(
        paths="\n".join(paths),
        diff=_truncate_diff(diff),
    )
    text = _run_claude(
        prompt,
        allowed_tools="Read Grep Glob",
        permission_mode="bypassPermissions",
        cwd=repo_root,
    )
    return _parse_check_response(text)


def update_docs(base: str, head: str) -> str:
    """Invoke claude to update ``docs/`` for code changes between base and head.

    Returns the summary text claude produced.
    """
    repo_root = get_repo_root()
    diff, paths = get_code_diff(base, head, repo_root)
    if not paths:
        return "no code changes — nothing to update"
    prompt = _UPDATE_PROMPT.format(
        paths="\n".join(paths),
        diff=_truncate_diff(diff),
    )
    return _run_claude(
        prompt,
        allowed_tools="Read Edit Write Grep Glob",
        permission_mode="acceptEdits",
        cwd=repo_root,
    )


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
    if result.up_to_date:
        console.print("[green]docs are up to date[/green]")
        return
    console.print("[red]docs are out of date — required changes:[/red]")
    for change in result.required_changes:
        console.print(f"  [bold]{change.file}[/bold]: {change.change}")
    sys.exit(1)


def _run_update() -> None:
    need_cmd("git")
    need_cmd("claude")
    args = _parse_args("update-docs")
    summary = update_docs(args.base, args.head)
    console.print(summary)


def check_main() -> None:
    run_with_error_handling(_run_check)


def update_main() -> None:
    run_with_error_handling(_run_update)
