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
as Rust crates with an Astro Starlight documentation site. You are preparing
release {tag}.

Below is GitHub's auto-generated changelog for this release: the pull requests
merged since the previous release, with their authors and links. Treat it as
the source of truth. You may use Read/Grep/Glob to inspect the repository when a
pull-request title is unclear, but do not invent changes that are not present.

Return a single JSON object as your final assistant message. No prose, no code
fence, no explanation, exactly this shape:

{{"title": "<headline>", "description": "<one sentence>", "notes": "<markdown>"}}

Requirements:
- "title": a concise, human headline for the release theme (max ~70 chars). Do
  not include the version number; it is shown separately.
- "description": a single sentence (max ~120 chars) summarizing the release for
  the changelog page.
- "notes": the release body in Markdown. Open with a short paragraph (one to
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
    text = run_claude(
        prompt,
        allowed_tools="Read Grep Glob",
        permission_mode="bypassPermissions",
        cwd=repo_root,
    )
    return _parse_release_content(text)


def _parse_release_content(text: str) -> ReleaseContent:
    """Parse and validate Claude's JSON response into a ReleaseContent."""
    stripped = strip_code_fence(text)
    try:
        payload = json.loads(stripped)
    except json.JSONDecodeError as e:
        raise ReleaseError(
            f"claude release notes were not valid JSON ({e}); text={text[:500]!r}"
        )
    if not isinstance(payload, dict):
        raise ReleaseError(f"claude release notes must be a JSON object: {payload!r}")

    values: dict[str, str] = {}
    for field in _FIELDS:
        value = payload.get(field)
        if not isinstance(value, str) or not value.strip():
            raise ReleaseError(
                f"claude release notes missing non-empty '{field}': {payload!r}"
            )
        values[field] = value.strip()
    return ReleaseContent(**values)
