"""Have Claude write the human-facing release content from the changes.

Given everything merged since the previous release (the commit subjects, the
code diff, and the user documentation diff), ask Claude to produce a short
headline, a one-line summary, and a self-contained Markdown body: a readable
list of the user-facing changes only, with no external links, pull-request
numbers, or author mentions. Internal work (refactors, crate reorganization,
dependency bumps, build changes, peppy's own tests) is left out.

The diffs are what makes the judgement reliable: a commit subject says where
the author worked, not what users get, and the code peppy generates into users'
projects or the test harness their tests call can hide behind subjects such as
"testing:" or "generator:". Truncation is applied to each diff separately so a
large code diff never crowds out the documentation diff, the strongest signal
that a change is user-facing.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from .claude import run_claude
from .cli import ReleaseError
from .docs import get_code_diff, get_docs_diff, truncate_diff
from .repo import get_commit_subjects

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

_NO_COMMITS = "(no commits since the last release)"
_NO_CODE_CHANGES = "(no code changes)"
_NO_DOCS_CHANGES = "(no user documentation changes)"
_NO_PREVIOUS_RELEASE = "(no previous release to diff against)"
_FIRST_RELEASE = (
    "none: no release has been published yet, so the commit subjects cover the "
    "full history and there is nothing to diff against"
)

_PROMPT = """\
You are writing the public release notes for "peppy", a developer tool shipped
as Rust crates with an Astro Starlight documentation site. This is release
{tag}; the previous release is {previous}.

Below are the changes merged since the previous release, in three parts: the
commit subjects (one per line), the unified diff of the code with its changed
paths, and the unified diff of the user documentation (the pages under
`docs/src/content/docs/`) with its changed paths. Together they are the only
source of truth. Do not invent changes they do not show.

Judge each change from the diffs, not from the wording of its commit subject.
Subjects are short and their prefixes describe where the author worked, not
what users get: a commit labelled "testing:", "generator:", or "refactor:" can
still add or change something users type, call, or see. A change to the user
documentation is strong evidence that the underlying change is user-facing:
every behaviour the documentation diff starts or stops describing belongs in
the notes.

Respond with the three fields "title" (a headline), "description" (one
sentence), and "notes" (Markdown).

Write for a user of the peppy tool, and include only changes such a user would
notice or care about: new or changed CLI commands and flags, the `peppy.json5`
configuration and schema, message and output formats, runtime behavior, the
code peppy generates into users' projects, and user-facing documentation.
Renaming, adding, or removing something a user types or sees is user-facing and
stays in, even when the commit is phrased as a rename: a CLI command or flag, a
`peppy.json5` field, the configuration file name, or a schema identifier (for
example "node/v1") all count. Before mentioning anything, ask "would a peppy
user notice or care about this?"; if not, leave it out entirely, across every
field.

Two parts of this repository sound internal but are products users depend on:
- The code generator (peppygen) and its templates. What it emits is compiled
  into users' nodes and imported by their tests: the production bindings, the
  `peppygen.mock` / `peppygen::mock` and `peppygen.fixtures` /
  `peppygen::fixtures` test surfaces, and the test harness they provide. A
  change to the generated code or to the harness API (a new configuration knob,
  fixture, method, or behaviour that user tests rely on) is user-facing. Only
  changes to how the generator itself is built or tested are internal.
- The testing features peppy gives its users. "Test-only" below means peppy's
  own test suite, never what users write their tests against.

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
unused or internal fields, dependency version bumps, build, CI, and
release-tooling changes, log-string tweaks, and changes confined to peppy's own
test suite (the tests, golden files, and test helpers in this repository that
verify peppy itself). Also omit version bumps, release commits, and merge
commits.

Commit subjects since the previous release:
{changes}

Changed code paths:
{code_paths}

Code diff:
{code_diff}

Changed user documentation paths:
{docs_paths}

User documentation diff:
{docs_diff}
"""


@dataclass(frozen=True)
class ReleaseContent:
    """The release fields Claude writes from the changes."""

    title: str
    description: str
    notes: str


@dataclass(frozen=True)
class PathDiff:
    """A unified diff restricted to a set of paths, with those paths listed."""

    text: str
    paths: tuple[str, ...]


@dataclass(frozen=True)
class ReleaseDiffs:
    """The code and user documentation diffs from the previous release."""

    previous_tag: str
    code: PathDiff
    docs: PathDiff


@dataclass(frozen=True)
class ReleaseChanges:
    """Everything Claude sees about a release: what was merged since the last one.

    ``diffs`` is None only when no release has been published yet: the commit
    subjects then cover the full history and there is no earlier state to diff
    against.
    """

    commit_subjects: tuple[str, ...]
    diffs: ReleaseDiffs | None


def collect_release_changes(
    previous_tag: str | None, head: str, repo_root: Path
) -> ReleaseChanges:
    """Gather the commit subjects and diffs between ``previous_tag`` and ``head``.

    ``previous_tag`` is the tag of the last published release, or None when
    there is none, in which case the commit subjects span the full history and
    no diff is collected.
    """
    subjects = tuple(get_commit_subjects(previous_tag, head))
    if previous_tag is None:
        return ReleaseChanges(commit_subjects=subjects, diffs=None)
    code_diff, code_paths = get_code_diff(previous_tag, head, repo_root)
    docs_diff, docs_paths = get_docs_diff(previous_tag, head, repo_root)
    return ReleaseChanges(
        commit_subjects=subjects,
        diffs=ReleaseDiffs(
            previous_tag=previous_tag,
            code=PathDiff(text=code_diff, paths=tuple(code_paths)),
            docs=PathDiff(text=docs_diff, paths=tuple(docs_paths)),
        ),
    )


def _render_paths(diff: PathDiff, empty: str) -> str:
    return "\n".join(diff.paths) or empty


def _render_diff(diff: PathDiff, empty: str) -> str:
    # Each diff is truncated on its own: the code diff between two releases
    # can exceed the budget, and must not push the documentation diff out.
    return truncate_diff(diff.text) if diff.text else empty


def _render_prompt(changes: ReleaseChanges, tag: str) -> str:
    subjects = "\n".join(f"- {s}" for s in changes.commit_subjects) or _NO_COMMITS
    diffs = changes.diffs
    if diffs is None:
        return _PROMPT.format(
            tag=tag,
            previous=_FIRST_RELEASE,
            changes=subjects,
            code_paths=_NO_PREVIOUS_RELEASE,
            code_diff=_NO_PREVIOUS_RELEASE,
            docs_paths=_NO_PREVIOUS_RELEASE,
            docs_diff=_NO_PREVIOUS_RELEASE,
        )
    return _PROMPT.format(
        tag=tag,
        previous=diffs.previous_tag,
        changes=subjects,
        code_paths=_render_paths(diffs.code, _NO_CODE_CHANGES),
        code_diff=_render_diff(diffs.code, _NO_CODE_CHANGES),
        docs_paths=_render_paths(diffs.docs, _NO_DOCS_CHANGES),
        docs_diff=_render_diff(diffs.docs, _NO_DOCS_CHANGES),
    )


def generate_release_content(
    changes: ReleaseChanges,
    tag: str,
    repo_root: Path,
) -> ReleaseContent:
    """Ask Claude to draft the release title, description, and notes.

    Claude runs with no tools (a pure transformation of ``changes``) and is
    instructed to emit a self-contained list of user-facing changes only, with
    no links, pull-request numbers, or author mentions.
    """
    prompt = _render_prompt(changes, tag)
    # No tools: the prompt already carries the commit subjects and both diffs.
    # Giving Claude repository access leads it to explore git history and answer
    # with an analysis narrative instead of the release content.
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
