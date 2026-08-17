"""Have Claude write the human-facing release content from the commit log.

Given the list of commit subjects since the previous release, ask Claude to
produce a short headline, a one-line summary, and a self-contained Markdown
body: a readable list of the user-facing changes only, with no external links,
pull-request numbers, or author mentions. Internal work (refactors, crate
reorganization, dependency bumps, build and codegen changes) is left out.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from .claude import run_claude
from .cli import ReleaseError

_FIELDS: tuple[str, ...] = ("title", "description", "notes")

# Enforced CLI-side via --json-schema. minLength keeps a blank field from
# passing validation, but whitespace-only strings still need the Python-side
# check in _parse_release_content.
_CONTENT_SCHEMA: dict = {
    "type": "object",
    "properties": {
        "title": {"type": "string", "minLength": 1},
        "description": {"type": "string", "minLength": 1},
        "notes": {"type": "string", "minLength": 1},
    },
    "required": ["title", "description", "notes"],
    "additionalProperties": False,
}

_PROMPT = """\
You are writing the public release notes for "peppy", a developer tool shipped
as Rust crates with an Astro Starlight documentation site. This is release
{tag}.

Below is the exhaustive list of commits merged since the previous release
(commit subjects, one per line). It is the only source of truth. Do not invent
changes that are not listed.

Respond with the three fields "title" (a headline), "description" (one
sentence), and "notes" (Markdown).

Write for a user of the peppy tool, and include only changes such a user would
notice or care about: new or changed CLI commands and flags, the `peppy.json5`
configuration and schema, message and output formats, runtime behavior, and
user-facing documentation. Renaming, adding, or removing something a user types
or sees is user-facing and stays in, even when the commit is phrased as a
rename: a CLI command or flag, a `peppy.json5` field, the configuration file
name, or a schema identifier (for example "node/v1") all count. Before
mentioning anything, ask "would a peppy user notice or care about this?"; if not,
leave it out entirely, across every field.

Field requirements:
- "title": a concise, human headline for the release theme (max ~70 chars),
  without the version number; it is shown separately. Headline only user-facing
  themes; never advertise internal work such as refactors or crate
  reorganization.
- "description": a single sentence (max ~120 chars) summarizing the release for
  users. Mention only user-facing themes, so the headline never promises changes
  the notes leave out.
- "notes": the release body in Markdown, as a bulleted list of the user-facing
  changes only, one bullet per change, each rewritten as a clear, self-contained
  sentence. Group related changes under short "### " subheadings when it improves
  readability. Do NOT include any links, URLs, pull-request or issue numbers (for
  example "#123"), commit hashes, author or "@" mentions, or a "Full Changelog"
  line.

Call out backward-incompatible changes. A user-facing change is breaking when a
user must update their `peppy.json5`, commands, scripts, or workflow to keep
working: for example a renamed or removed CLI command or flag, a renamed
configuration file or field, a changed schema identifier (such as the
slash-separated `node/v1`), or a changed output or message format that users
parse. Collect every such change under a single "### Breaking changes"
subheading placed first in the notes, before all other subheadings, and make
each bullet state what changed and what the user must do. List a breaking change
only there, never also under a topical subheading. Omit the "### Breaking
changes" subheading entirely when there are none.

Leave out every internal change, even when it spans many commits: code refactors
and restructuring, renames and reorganization confined to internal code (crates,
modules, and private fields or APIs that users never reference), removal of
unused or internal fields, dependency version bumps, build, CI, code-generator,
and release-tooling changes, test-only changes, and log-string tweaks. Also omit
version bumps, release commits, and merge commits.

Commits since the previous release:
{changes}
"""


@dataclass(frozen=True)
class ReleaseContent:
    """The release fields Claude writes from the commit log."""

    title: str
    description: str
    notes: str


def generate_release_content(
    commit_subjects: list[str],
    tag: str,
    repo_root: Path,
) -> ReleaseContent:
    """Ask Claude to draft the release title, description, and notes.

    ``commit_subjects`` are the git commit subjects since the previous release.
    Claude runs with no tools (a pure transformation) and is instructed to emit
    a self-contained list of user-facing changes only, with no links,
    pull-request numbers, or author mentions.
    """
    changes = "\n".join(f"- {subject}" for subject in commit_subjects) or (
        "(no commits since the last release)"
    )
    prompt = _PROMPT.format(tag=tag, changes=changes)
    # No tools: this is a pure transformation of the commit list. Giving Claude
    # repository access leads it to explore git history and answer with an
    # analysis narrative instead of the release content.
    payload = run_claude(
        prompt,
        allowed_tools="",
        permission_mode="bypassPermissions",
        cwd=repo_root,
        json_schema=_CONTENT_SCHEMA,
        tools="",
        effort="xhigh",
    )
    return _parse_release_content(payload)


def _parse_release_content(payload: dict) -> ReleaseContent:
    """Validate Claude's structured output into a ReleaseContent."""
    values: dict[str, str] = {}
    for field in _FIELDS:
        value = payload.get(field)
        if not isinstance(value, str) or not value.strip():
            raise ReleaseError(
                f"claude release notes missing non-empty '{field}': {payload!r}"
            )
        values[field] = value.strip()
    return ReleaseContent(**values)
