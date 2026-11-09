"""Have Claude draft the human-facing release content, and the version it
gives the release, from the changes.

Given everything merged since the previous release (the commit subjects, the
code diff, and the user documentation diff), ask Claude to produce a short
headline, a one-line summary, and a self-contained Markdown body: a readable
list of the user-facing changes only, with no external links, pull-request
numbers, or author mentions. Internal work (refactors, crate reorganization,
dependency bumps, build changes, peppy's own tests) is left out.

Claude also reports whether the notes list a breaking change, which picks the
version of the release (see release_version.py): the next minor version when
they do, the next patch version when they do not. The notes and the report
must agree, since the notes are published unreviewed and the version tells
users whether they must change anything.

The diffs are what makes the judgement reliable: a commit subject says where
the author worked, not what users get, and the code peppy generates into users'
projects or the test harness their tests call can hide behind subjects such as
"testing:" or "generator:". Truncation is applied to each diff separately so a
large code diff never crowds out the documentation diff, the strongest signal
that a change is user-facing.

A release can be cut only to pin newer commits of the default hubs, with no
user-facing change in peppy itself. Such a release gets fixed content that
says so (`hub_pins_only_content`): without asking Claude when neither the code
diff nor the user documentation diff has a path, and when Claude reports that
no change is user-facing. Such a release lists no breaking change.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from .claude import run_claude
from .cli import ReleaseError, console
from .docs import get_code_diff, get_docs_diff, truncate_diff
from .hub_ci import resolve
from .release_version import ReleaseVersion, next_release_version
from .repo import get_commit_subjects

_FIELDS: tuple[str, ...] = ("title", "description", "notes")

_BREAKING_VERDICT = "has_breaking_changes"
_USER_FACING_VERDICT = "has_user_facing_changes"

# The subheading the prompt asks for over the breaking changes of the notes,
# and only then.
_BREAKING_CHANGES_HEADING = "### Breaking changes"

# Enforced CLI-side via --json-schema. The text fields have no minLength: they
# are empty when Claude reports no user-facing change. _parse_drafted_release
# requires them non-empty, whitespace aside, in the other case, and empty in
# that one. The verdicts come last, in the order the prompt asks for them:
# with a verdict first, Claude can write its reasoning about it into the notes.
_CONTENT_SCHEMA: dict = {
    "type": "object",
    "properties": {
        "title": {"type": "string"},
        "description": {"type": "string"},
        "notes": {"type": "string"},
        _BREAKING_VERDICT: {"type": "boolean"},
        _USER_FACING_VERDICT: {"type": "boolean"},
    },
    "required": [*_FIELDS, _BREAKING_VERDICT, _USER_FACING_VERDICT],
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
as Rust crates with an Astro Starlight documentation site. The previous release
is {previous}.

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
  changes only. Keep it short: one bullet per capability a user gains or loses,
  written as a single plain sentence of at most about 30 words that says what
  the user can now do (or must do differently), not how it is implemented or
  enforced. A capability's details (its options, defaults, error cases,
  guardrails, and per-language spellings) stay in the documentation unless
  the sentence has room for one of them; name an API once, in whichever
  language's spelling reads most naturally, rather than quoting both the
  Python and the Rust forms. Add
  short "### " subheadings only when the release spans several unrelated
  themes (plus the "### Breaking changes" subheading described below whenever
  it applies); a release with one theme is a flat list. Do NOT include any
  links, URLs, pull-request or issue numbers (for example "#123"), commit
  hashes, author or "@" mentions, or a "Full Changelog" line.

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

Also respond with the field "has_breaking_changes": true when the notes hold the
"### Breaking changes" subheading, and false when they do not. It picks the
version of this release: the next minor version when it is true, and the next
patch version when it is false.

Leave out every internal change, even when it spans many commits: code refactors
and restructuring, renames and reorganization confined to internal code (crates,
modules, and private fields or APIs that users never reference), removal of
unused or internal fields, dependency version bumps, build, CI, and
release-tooling changes, log-string tweaks, and changes confined to peppy's own
test suite (the tests, golden files, and test helpers in this repository that
verify peppy itself). Also omit version bumps, release commits, and merge
commits.

Every peppy release also pins newer commits of peppy's default hubs, the
repositories that publish the nodes, contracts, and launchers users run, so a
release can be cut for those pins alone. Also respond with the field
"has_user_facing_changes": true when the notes list at least one change, and
false when no change above is user-facing. When it is false, leave "title",
"description", and "notes" as empty strings and "has_breaking_changes" false:
the release then gets fixed notes that say it only pins newer hub commits.

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
class DraftedRelease:
    """A release as drafted from its changes: its version and its content."""

    version: ReleaseVersion
    content: ReleaseContent


@dataclass(frozen=True)
class PathDiff:
    """A unified diff restricted to a set of paths, with those paths listed."""

    text: str
    paths: tuple[str, ...]


@dataclass(frozen=True)
class ReleaseDiffs:
    """The code and user documentation diffs from the previous release."""

    previous: ReleaseVersion
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

    @property
    def previous(self) -> ReleaseVersion | None:
        """The latest published release, or None when no release has been
        published yet."""
        if self.diffs is None:
            return None
        return self.diffs.previous

    @property
    def has_code_or_docs_changes(self) -> bool:
        """Whether the code diff or the user documentation diff has a path.

        Without a previous release there is no diff, and the full history
        counts as changed.
        """
        if self.diffs is None:
            return True
        return bool(self.diffs.code.paths or self.diffs.docs.paths)


def hub_pins_only_content(tag: str) -> ReleaseContent:
    """The content of release *tag* when it has no user-facing change in peppy
    itself, and only pins newer commits of the default hubs."""
    return ReleaseContent(
        title="Newer content from the default hubs",
        description=(
            "This release only pins newer commits of the default hubs; Peppy "
            "itself has no user-facing changes."
        ),
        notes=(
            "- This release only pins newer commits of the default hubs: Peppy "
            "itself has no user-facing changes.\n"
            '- Once you install it, every default hub on `ref: "@{peppy-release}"` '
            f"reads the tag `{resolve.hub_release_tag(tag)}`."
        ),
    )


def _hub_pins_only_release(previous: ReleaseVersion | None) -> DraftedRelease:
    """The release that follows *previous* when it has no user-facing change
    in peppy itself: it lists no breaking change, so it takes the next patch
    version."""
    version = next_release_version(previous, breaking=False)
    return DraftedRelease(version=version, content=hub_pins_only_content(version.tag))


def collect_release_changes(
    previous: ReleaseVersion | None, head: str, repo_root: Path
) -> ReleaseChanges:
    """Gather the commit subjects and diffs between ``previous`` and ``head``.

    ``previous`` is the last published release, or None when there is none,
    in which case the commit subjects span the full history and no diff is
    collected.
    """
    if previous is None:
        subjects = tuple(get_commit_subjects(None, head))
        return ReleaseChanges(commit_subjects=subjects, diffs=None)
    subjects = tuple(get_commit_subjects(previous.tag, head))
    code_diff, code_paths = get_code_diff(previous.tag, head, repo_root)
    docs_diff, docs_paths = get_docs_diff(previous.tag, head, repo_root)
    return ReleaseChanges(
        commit_subjects=subjects,
        diffs=ReleaseDiffs(
            previous=previous,
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


def _render_prompt(changes: ReleaseChanges) -> str:
    subjects = "\n".join(f"- {s}" for s in changes.commit_subjects) or _NO_COMMITS
    diffs = changes.diffs
    if diffs is None:
        return _PROMPT.format(
            previous=_FIRST_RELEASE,
            changes=subjects,
            code_paths=_NO_PREVIOUS_RELEASE,
            code_diff=_NO_PREVIOUS_RELEASE,
            docs_paths=_NO_PREVIOUS_RELEASE,
            docs_diff=_NO_PREVIOUS_RELEASE,
        )
    return _PROMPT.format(
        previous=diffs.previous.tag,
        changes=subjects,
        code_paths=_render_paths(diffs.code, _NO_CODE_CHANGES),
        code_diff=_render_diff(diffs.code, _NO_CODE_CHANGES),
        docs_paths=_render_paths(diffs.docs, _NO_DOCS_CHANGES),
        docs_diff=_render_diff(diffs.docs, _NO_DOCS_CHANGES),
    )


def generate_release_draft(changes: ReleaseChanges, repo_root: Path) -> DraftedRelease:
    """Draft the release title, description, and notes, and the version they
    give the release.

    When neither the code nor the user documentation changed, the release only
    pins newer hub commits, and its content says so without asking Claude.
    Otherwise Claude runs with no tools (a pure transformation of ``changes``)
    and is instructed to emit a self-contained list of user-facing changes
    only, with no links, pull-request numbers, or author mentions, and to
    report whether it lists a breaking change, or to report that no change is
    user-facing.
    """
    if not changes.has_code_or_docs_changes:
        console.print(
            "Neither the code nor the user documentation changed since the "
            "previous release: the release only pins newer hub commits."
        )
        return _hub_pins_only_release(changes.previous)

    console.print("Asking Claude to write the release notes...")
    prompt = _render_prompt(changes)
    # No tools: the prompt already carries the commit subjects and both diffs.
    # Giving Claude repository access leads it to explore git history and answer
    # with an analysis narrative instead of the release content.
    payload = run_claude(
        prompt,
        allowed_tools="",
        permission_mode="bypassPermissions",
        cwd=repo_root,
        json_schema=_CONTENT_SCHEMA,
        activity="drafting the release notes",
        tools="",
        effort="xhigh",
    )
    return _parse_drafted_release(payload, changes.previous)


def _parse_drafted_release(
    payload: dict, previous: ReleaseVersion | None
) -> DraftedRelease:
    """Parse Claude's structured output into the release that follows
    *previous*, the latest published release, or None without one.

    A report of no user-facing change gives the hub-pins-only release, and is
    refused when Claude also wrote a text field or reported a breaking
    change. A report of a breaking change is refused unless the notes hold
    the breaking changes subheading, and the subheading unless the report
    says so. The notes are published unreviewed, and the version follows from
    the report, so a contradictory answer must decide neither.
    """
    has_user_facing_changes = _require_verdict(payload, _USER_FACING_VERDICT)
    has_breaking_changes = _require_verdict(payload, _BREAKING_VERDICT)
    if not has_user_facing_changes:
        _require_no_text_fields(payload)
        if has_breaking_changes:
            raise ReleaseError(
                f"claude reported no user-facing change but a breaking change, "
                f"which is user-facing: {payload!r}"
            )
        return _hub_pins_only_release(previous)

    values: dict[str, str] = {}
    for field in _FIELDS:
        value = payload.get(field)
        if not isinstance(value, str) or not value.strip():
            raise ReleaseError(
                f"claude release notes missing non-empty '{field}': {payload!r}"
            )
        values[field] = value.strip()
    content = ReleaseContent(**values)
    _require_breaking_verdict_of_the_notes(has_breaking_changes, content.notes)
    return DraftedRelease(
        version=next_release_version(previous, breaking=has_breaking_changes),
        content=content,
    )


def _lists_breaking_changes(notes: str) -> bool:
    """Whether *notes* hold the breaking changes subheading, whatever the case
    of its letters."""
    heading = _BREAKING_CHANGES_HEADING.casefold()
    return any(line.strip().casefold() == heading for line in notes.splitlines())


def _require_breaking_verdict_of_the_notes(
    has_breaking_changes: bool, notes: str
) -> None:
    """Refuse a breaking change verdict that the notes contradict."""
    lists_breaking_changes = _lists_breaking_changes(notes)
    if has_breaking_changes and not lists_breaking_changes:
        raise ReleaseError(
            f"claude reported a breaking change, but the notes have no "
            f"'{_BREAKING_CHANGES_HEADING}' subheading: {notes!r}"
        )
    if lists_breaking_changes and not has_breaking_changes:
        raise ReleaseError(
            f"claude reported no breaking change, but the notes have a "
            f"'{_BREAKING_CHANGES_HEADING}' subheading: {notes!r}"
        )


def _require_verdict(payload: dict, field: str) -> bool:
    """The boolean verdict *field* of Claude's structured output."""
    verdict = payload.get(field)
    if not isinstance(verdict, bool):
        raise ReleaseError(
            f"claude release notes missing boolean '{field}': {payload!r}"
        )
    return verdict


def _require_no_text_fields(payload: dict) -> None:
    """Refuse a report of no user-facing change that also writes a text field."""
    for field in _FIELDS:
        value = payload.get(field)
        if not isinstance(value, str) or value.strip():
            raise ReleaseError(
                f"claude reported no user-facing change but did not leave "
                f"'{field}' empty: {payload!r}"
            )
