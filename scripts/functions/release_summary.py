"""Have Claude write the human-facing release content from a changelog.

Given GitHub's auto-generated "What's Changed" changelog for a release (the
merged pull requests since the previous release, with author and PR links),
ask Claude to produce a short headline, a one-line summary, and a polished
Markdown body that keeps the original pull-request list intact.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

from .claude import run_claude, strip_code_fence
from .cli import ReleaseError

_FIELDS: tuple[str, ...] = ("title", "description", "notes")

_PROMPT = """\
You are writing the public release notes for "peppy", a developer tool shipped
as Rust crates with an Astro Starlight documentation site. This is release
{tag}.

Below is GitHub's auto-generated changelog for this release: the pull requests
merged since the previous release, with their authors and links. It is the only
source of truth. Do not invent changes that are not listed.

Respond with a single JSON object and nothing else. Do not explain your
reasoning, do not narrate, do not add any text before or after the object, and
do not wrap it in a code fence. Your entire response must be valid JSON that a
strict parser accepts, exactly this shape:

{{"title": "<headline>", "description": "<one sentence>", "notes": "<markdown>"}}

Field requirements:
- "title": a concise, human headline for the release theme (max ~70 chars),
  without the version number; it is shown separately.
- "description": a single sentence (max ~120 chars) summarizing the release for
  the changelog page.
- "notes": the release body in Markdown. Begin with a short paragraph (one to
  three sentences) summarizing the most important changes, then include the
  provided "What's Changed" list verbatim, preserving every pull-request link,
  author mention, and the "Full Changelog" line.

GitHub changelog:
{changelog}
"""


@dataclass(frozen=True)
class ReleaseContent:
    """The release fields Claude writes from the changelog."""

    title: str
    description: str
    notes: str


def generate_release_content(
    changelog_markdown: str,
    tag: str,
    repo_root: Path,
) -> ReleaseContent:
    """Ask Claude to draft the release title, description, and notes.

    ``changelog_markdown`` is GitHub's generate-notes output for the release.
    Claude runs read-only (it may inspect the repo but cannot edit it).
    """
    changelog = changelog_markdown.strip() or "(no merged pull requests since the last release)"
    prompt = _PROMPT.format(tag=tag, changelog=changelog)
    # No tools: this is a pure transformation of the changelog. Giving Claude
    # repository access leads it to explore git history and answer with an
    # analysis narrative instead of the JSON object.
    text = run_claude(
        prompt,
        allowed_tools="",
        permission_mode="bypassPermissions",
        cwd=repo_root,
        tools="",
    )
    return _parse_release_content(text)


def _parse_release_content(text: str) -> ReleaseContent:
    """Parse and validate Claude's JSON response into a ReleaseContent."""
    payload = _decode_json_object(text)

    values: dict[str, str] = {}
    for field in _FIELDS:
        value = payload.get(field)
        if not isinstance(value, str) or not value.strip():
            raise ReleaseError(
                f"claude release notes missing non-empty '{field}': {payload!r}"
            )
        values[field] = value.strip()
    return ReleaseContent(**values)


def _decode_json_object(text: str) -> dict:
    """Decode Claude's response into a JSON object, tolerating a stray preamble.

    Tries the whole (de-fenced) response first, then falls back to the first
    balanced ``{...}`` substring. Raises ReleaseError if neither is a JSON
    object.
    """
    stripped = strip_code_fence(text)
    candidates = [stripped]
    extracted = _first_json_object(stripped)
    if extracted is not None and extracted != stripped:
        candidates.append(extracted)

    for candidate in candidates:
        try:
            payload = json.loads(candidate)
        except json.JSONDecodeError:
            continue
        if isinstance(payload, dict):
            return payload
        raise ReleaseError(f"claude release notes must be a JSON object: {payload!r}")

    raise ReleaseError(
        f"claude release notes were not valid JSON; text={text[:500]!r}"
    )


def _first_json_object(text: str) -> str | None:
    """Return the first balanced top-level ``{...}`` substring, or None.

    String-aware, so braces inside JSON string values do not unbalance the scan.
    """
    start = text.find("{")
    if start == -1:
        return None
    depth = 0
    in_string = False
    escaped = False
    for index in range(start, len(text)):
        char = text[index]
        if in_string:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                in_string = False
            continue
        if char == '"':
            in_string = True
        elif char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                return text[start : index + 1]
    return None
