"""Shared helper for driving the ``claude`` CLI from the release scripts.

Both the docs freshness pipeline (``docs.py``) and the release-notes
generator (``release_summary.py``) invoke Claude the same way: run
``claude -p`` with a pinned model and effort, capture the JSON envelope, and
return the final assistant text. This module is the single source of truth for
that invocation so the model pin and response parsing live in one place.

Requires ``claude`` on PATH, already authenticated in the invoking environment.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

from .cli import ReleaseError

# Pinned for reproducibility. The prompt and its inputs are already
# deterministic for a given input, so the only sources of run-to-run drift are
# which model answers and at what effort. Pinning both removes that systematic
# drift: the same input feeds the same model at the same effort every time,
# instead of silently changing behaviour whenever the CLI's default model or
# effort is bumped.
#
# This is not bit-for-bit determinism (unattainable with an LLM: sampling is
# never fully reproducible and temperature is not configurable on Opus 4.7+).
# Callers are built to tolerate the residual variance.
#
# Use the full model id (not the "opus" alias) so the pin does not follow the
# moving "latest" pointer. "max" is the highest effort tier ("ultracode").
CLAUDE_MODEL = "claude-opus-4-8"
CLAUDE_EFFORT = "max"


def run_claude(
    prompt: str,
    *,
    allowed_tools: str,
    permission_mode: str,
    cwd: Path,
) -> str:
    """Run ``claude -p`` and return the final assistant text."""
    # Prompt is piped via stdin rather than passed as an argv element: prompts
    # can embed large diffs or changelogs that exceed ARG_MAX on Linux
    # (~128KB) and would trigger E2BIG.
    cmd = [
        "claude",
        "-p",
        "--model",
        CLAUDE_MODEL,
        "--effort",
        CLAUDE_EFFORT,
        "--output-format",
        "json",
        "--permission-mode",
        permission_mode,
        "--allowed-tools",
        allowed_tools,
    ]
    result = subprocess.run(
        cmd, cwd=cwd, input=prompt, capture_output=True, text=True
    )
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


def strip_code_fence(text: str) -> str:
    """Strip a leading/trailing Markdown code fence from a model response."""
    text = text.strip()
    if not text.startswith("```"):
        return text
    lines = text.splitlines()[1:]
    if lines and lines[-1].strip().startswith("```"):
        lines = lines[:-1]
    return "\n".join(lines).strip()
