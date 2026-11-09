"""Tests for functions.release_summary module."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import patch

import pytest

from functions.cli import ReleaseError
from functions.release_summary import (
    _CONTENT_SCHEMA,
    DraftedRelease,
    PathDiff,
    ReleaseChanges,
    ReleaseContent,
    ReleaseDiffs,
    _parse_drafted_release,
    collect_release_changes,
    generate_release_draft,
    hub_pins_only_content,
)
from functions.release_version import ReleaseVersion

# The latest published release, and the versions of the release after it.
PREVIOUS = ReleaseVersion(minor=11, patch=1)
NEXT_PATCH = ReleaseVersion(minor=11, patch=2)
NEXT_MINOR = ReleaseVersion(minor=12, patch=0)

# What Claude answers for a release with user-facing changes and no breaking
# change.
DRAFTED = {
    "has_breaking_changes": False,
    "has_user_facing_changes": True,
    "title": "T",
    "description": "D",
    "notes": "N",
}

BREAKING_NOTES = (
    "### Breaking changes\n"
    "- `peppy node run` is now `peppy node start`: update your scripts.\n"
    "### Nodes\n"
    "- Nodes restart on failure."
)

# What Claude answers for a release with a breaking change.
DRAFTED_BREAKING = {**DRAFTED, "has_breaking_changes": True, "notes": BREAKING_NOTES}

# What Claude answers for a release with no user-facing change.
NO_USER_FACING_CHANGE = {
    "has_breaking_changes": False,
    "has_user_facing_changes": False,
    "title": "",
    "description": "",
    "notes": "",
}

NO_DIFF = PathDiff("", ())
CODE_DIFF = PathDiff("+fn main() {}", ("crates/peppy/src/main.rs",))
DOCS_DIFF = PathDiff("+## The clock", ("docs/src/content/docs/x.mdx",))


def _changes(
    subjects: tuple[str, ...] = ("fix(apptainer): pre-flight bind mounts",),
    code: PathDiff = CODE_DIFF,
    docs: PathDiff = DOCS_DIFF,
) -> ReleaseChanges:
    return ReleaseChanges(
        commit_subjects=subjects,
        diffs=ReleaseDiffs(previous=PREVIOUS, code=code, docs=docs),
    )


def _hub_pins_only_release(version: ReleaseVersion) -> DraftedRelease:
    return DraftedRelease(version=version, content=hub_pins_only_content(version.tag))


# --- _CONTENT_SCHEMA ---


def test_content_schema_requires_both_verdicts_and_every_text_field() -> None:
    # The Python parser and the CLI-side schema must not drift.
    # The verdicts come last, after the fields they decide about, in the order
    # the prompt asks for them.
    assert list(_CONTENT_SCHEMA["properties"]) == [
        "title",
        "description",
        "notes",
        "has_breaking_changes",
        "has_user_facing_changes",
    ]
    assert _CONTENT_SCHEMA["required"] == list(_CONTENT_SCHEMA["properties"])
    for verdict in ("has_breaking_changes", "has_user_facing_changes"):
        assert _CONTENT_SCHEMA["properties"][verdict] == {"type": "boolean"}
    # A text field is empty when Claude reports no user-facing change, so the
    # schema must accept an empty string.
    for field in ("title", "description", "notes"):
        assert _CONTENT_SCHEMA["properties"][field] == {"type": "string"}


# --- hub_pins_only_content ---


def test_hub_pins_only_content_names_the_hub_tag_of_the_release() -> None:
    content = hub_pins_only_content("v0.12.0")

    assert "only pins newer commits of the default hubs" in content.description
    assert "only pins newer commits of the default hubs" in content.notes
    assert "`peppy-release/v0.12.0`" in content.notes
    # Published unreviewed, as every release body is: it follows the release
    # notes rules (a bulleted list, no em-dash).
    assert all(line.startswith("- ") for line in content.notes.splitlines())
    for text in (content.title, content.description, content.notes):
        assert text.strip()
        assert "\u2014" not in text


# --- _parse_drafted_release ---


def test_parse_drafted_release_valid() -> None:
    payload = {
        **DRAFTED,
        "title": "Topics hardening",
        "description": "Fixed deadlocks.",
        "notes": "## What's Changed\n- x",
    }
    assert _parse_drafted_release(payload, PREVIOUS) == DraftedRelease(
        version=NEXT_PATCH,
        content=ReleaseContent(
            title="Topics hardening",
            description="Fixed deadlocks.",
            notes="## What's Changed\n- x",
        ),
    )


def test_parse_drafted_release_strips_whitespace() -> None:
    payload = {**DRAFTED, "title": "  T  ", "description": " D ", "notes": " N "}
    assert _parse_drafted_release(payload, PREVIOUS).content == ReleaseContent(
        "T", "D", "N"
    )


def test_a_breaking_change_takes_the_next_minor_version() -> None:
    assert _parse_drafted_release(DRAFTED_BREAKING, PREVIOUS) == DraftedRelease(
        version=NEXT_MINOR, content=ReleaseContent("T", "D", BREAKING_NOTES)
    )


def test_the_breaking_changes_subheading_is_read_whatever_the_case() -> None:
    notes = "### Breaking Changes\n- `peppy node run` is now `peppy node start`."
    payload = {**DRAFTED_BREAKING, "notes": notes}

    assert _parse_drafted_release(payload, PREVIOUS).version == NEXT_MINOR


def test_the_first_release_is_v0_1_0_whatever_it_breaks() -> None:
    for payload in (DRAFTED, DRAFTED_BREAKING, NO_USER_FACING_CHANGE):
        assert _parse_drafted_release(payload, None).version.tag == "v0.1.0"


def test_parse_drafted_release_refuses_a_breaking_verdict_without_its_subheading() -> (
    None
):
    # The version follows from the verdict, and users read the notes: the two
    # must not disagree.
    payload = {**DRAFTED_BREAKING, "notes": "- Breaking changes: none of note."}
    with pytest.raises(
        ReleaseError,
        match="reported a breaking change, but the notes have no "
        "'### Breaking changes' subheading",
    ):
        _parse_drafted_release(payload, PREVIOUS)


def test_parse_drafted_release_refuses_the_subheading_without_a_breaking_verdict() -> (
    None
):
    payload = {**DRAFTED, "notes": BREAKING_NOTES}
    with pytest.raises(
        ReleaseError,
        match="reported no breaking change, but the notes have a "
        "'### Breaking changes' subheading",
    ):
        _parse_drafted_release(payload, PREVIOUS)


def test_parse_drafted_release_takes_a_mention_of_breaking_changes_as_no_subheading() -> (
    None
):
    # Only the subheading itself, alone on its line, lists breaking changes.
    payload = {**DRAFTED, "notes": "- Fixes the breaking changes subheading of logs."}
    assert _parse_drafted_release(payload, PREVIOUS).version == NEXT_PATCH


@pytest.mark.parametrize("missing", ["title", "description", "notes"])
def test_parse_drafted_release_missing_field(missing: str) -> None:
    payload = dict(DRAFTED)
    del payload[missing]
    with pytest.raises(ReleaseError, match=f"missing non-empty '{missing}'"):
        _parse_drafted_release(payload, PREVIOUS)


@pytest.mark.parametrize("field", ["title", "description", "notes"])
def test_parse_drafted_release_blank_field(field: str) -> None:
    # The schema accepts an empty string, since a release with no user-facing
    # change leaves every text field empty; the Python-side check refuses a
    # blank field of a release with user-facing changes.
    payload = dict(DRAFTED)
    payload[field] = "   "
    with pytest.raises(ReleaseError, match=f"missing non-empty '{field}'"):
        _parse_drafted_release(payload, PREVIOUS)


def test_parse_drafted_release_non_string_field() -> None:
    payload: dict[str, object] = {**DRAFTED, "notes": 123}
    with pytest.raises(ReleaseError, match="missing non-empty 'notes'"):
        _parse_drafted_release(payload, PREVIOUS)


def test_parse_drafted_release_without_user_facing_changes_pins_hubs_only() -> None:
    # It lists no breaking change, so it takes the next patch version, which
    # its notes name.
    assert _parse_drafted_release(NO_USER_FACING_CHANGE, PREVIOUS) == (
        _hub_pins_only_release(NEXT_PATCH)
    )


def test_parse_drafted_release_takes_whitespace_as_an_empty_field() -> None:
    payload = {**NO_USER_FACING_CHANGE, "notes": " \n "}
    assert _parse_drafted_release(payload, PREVIOUS) == _hub_pins_only_release(
        NEXT_PATCH
    )


@pytest.mark.parametrize("field", ["title", "description", "notes"])
def test_parse_drafted_release_refuses_text_without_user_facing_changes(
    field: str,
) -> None:
    # A contradictory answer must not decide what the unreviewed notes say.
    payload = {**NO_USER_FACING_CHANGE, field: "Internal improvements."}
    with pytest.raises(
        ReleaseError,
        match=f"no user-facing change but did not leave '{field}' empty",
    ):
        _parse_drafted_release(payload, PREVIOUS)


@pytest.mark.parametrize("field", ["title", "description", "notes"])
def test_parse_drafted_release_refuses_a_missing_field_without_user_facing_changes(
    field: str,
) -> None:
    payload = dict(NO_USER_FACING_CHANGE)
    del payload[field]
    with pytest.raises(
        ReleaseError,
        match=f"no user-facing change but did not leave '{field}' empty",
    ):
        _parse_drafted_release(payload, PREVIOUS)


def test_parse_drafted_release_refuses_a_breaking_change_without_user_facing_changes() -> (
    None
):
    # A breaking change is user-facing, and must not decide the version alone.
    payload = {**NO_USER_FACING_CHANGE, "has_breaking_changes": True}
    with pytest.raises(
        ReleaseError, match="no user-facing change but a breaking change"
    ):
        _parse_drafted_release(payload, PREVIOUS)


@pytest.mark.parametrize(
    "verdict_field", ["has_user_facing_changes", "has_breaking_changes"]
)
@pytest.mark.parametrize("verdict", [None, "false", 0])
def test_parse_drafted_release_requires_boolean_verdicts(
    verdict_field: str, verdict: object
) -> None:
    payload: dict[str, object] = {**DRAFTED, verdict_field: verdict}
    if verdict is None:
        del payload[verdict_field]
    with pytest.raises(ReleaseError, match=f"missing boolean '{verdict_field}'"):
        _parse_drafted_release(payload, PREVIOUS)


# --- generate_release_draft (mocked run_claude) ---


def _drafting_prompt(changes: ReleaseChanges, cwd: Path) -> str:
    """The prompt generate_release_draft hands Claude to draft *changes*."""
    prompts: list[str] = []

    def fake_run_claude(prompt: str, **kwargs: object) -> dict:
        prompts.append(prompt)
        return DRAFTED

    with patch("functions.release_summary.run_claude", side_effect=fake_run_claude):
        generate_release_draft(changes, cwd)
    return prompts[-1]


def test_generate_release_draft_runs_claude_without_tools(tmp_path: Path) -> None:
    captured: dict[str, object] = {}

    def _fake_run_claude(
        prompt: str,
        *,
        allowed_tools: str,
        permission_mode: str,
        cwd: Path,
        json_schema: dict,
        activity: str,
        tools: str | None = None,
        effort: str = "max",
    ) -> dict:
        captured["prompt"] = prompt
        captured["allowed_tools"] = allowed_tools
        captured["permission_mode"] = permission_mode
        captured["cwd"] = cwd
        captured["json_schema"] = json_schema
        captured["activity"] = activity
        captured["tools"] = tools
        captured["effort"] = effort
        return DRAFTED

    changes = _changes(
        subjects=("fix(apptainer): pre-flight bind mounts", "refactor: extract helper")
    )
    with patch("functions.release_summary.run_claude", side_effect=_fake_run_claude):
        result = generate_release_draft(changes, tmp_path)

    assert result == DraftedRelease(
        version=NEXT_PATCH, content=ReleaseContent("T", "D", "N")
    )
    # Pure transformation: tools disabled so Claude cannot explore and ramble.
    assert captured["tools"] == ""
    assert captured["allowed_tools"] == ""
    assert captured["effort"] == "xhigh"
    assert captured["cwd"] == tmp_path
    # The heartbeat names the work while Claude drafts.
    assert captured["activity"] == "drafting the release notes"
    # The response shape is enforced CLI-side.
    assert captured["json_schema"] == _CONTENT_SCHEMA
    # The previous tag and the commit subjects are interpolated. The version of
    # the release follows from the answer, so the prompt cannot name it.
    prompt = captured["prompt"]
    assert isinstance(prompt, str)
    assert "site. The previous release\nis v0.11.1.\n" in prompt
    assert "v0.11.2" not in prompt
    assert "v0.12.0" not in prompt
    assert (
        "- fix(apptainer): pre-flight bind mounts\n- refactor: extract helper" in prompt
    )
    # Claude can report that no change is user-facing instead of drafting.
    assert '"has_user_facing_changes": true when the notes list' in prompt
    # Claude reports whether the notes list a breaking change, which picks
    # the version.
    assert (
        '"has_breaking_changes": true when the notes hold the\n"### Breaking changes" subheading'
        in prompt
    )
    assert "the next minor version when it is true" in prompt


def test_generate_release_draft_gives_a_breaking_release_the_next_minor_version(
    tmp_path: Path,
) -> None:
    with patch("functions.release_summary.run_claude", return_value=DRAFTED_BREAKING):
        result = generate_release_draft(_changes(), tmp_path)

    assert result.version == NEXT_MINOR
    assert result.content.notes == BREAKING_NOTES


def test_generate_release_draft_feeds_both_diffs_and_their_paths(
    tmp_path: Path,
) -> None:
    changes = _changes(
        code=PathDiff(
            "+fn main() {}",
            ("crates/peppy/src/main.rs", "crates/peppy/src/cli.rs"),
        ),
        docs=PathDiff("+## The clock", ("docs/src/content/docs/x.mdx",)),
    )

    prompt = _drafting_prompt(changes, tmp_path)
    assert (
        "Changed code paths:\ncrates/peppy/src/main.rs\ncrates/peppy/src/cli.rs\n"
        in prompt
    )
    assert "Code diff:\n+fn main() {}\n" in prompt
    assert "Changed user documentation paths:\ndocs/src/content/docs/x.mdx\n" in prompt
    assert "User documentation diff:\n+## The clock\n" in prompt


def test_generate_release_draft_truncates_each_diff_separately(
    tmp_path: Path,
) -> None:
    changes = _changes(
        code=PathDiff("c" * 500_000, ("crates/peppy/src/main.rs",)),
        docs=PathDiff("d" * 1000, ("docs/src/content/docs/x.mdx",)),
    )

    prompt = _drafting_prompt(changes, tmp_path)
    # The oversized code diff is cut, and the docs diff survives untouched.
    assert prompt.count("diff truncated") == 1
    assert "d" * 1000 in prompt
    assert "c" * 500_000 not in prompt


def test_generate_release_draft_names_an_empty_code_diff(tmp_path: Path) -> None:
    changes = _changes(subjects=(), code=NO_DIFF)

    prompt = _drafting_prompt(changes, tmp_path)
    assert "(no commits since the last release)" in prompt
    assert prompt.count("(no code changes)") == 2
    assert "(no user documentation changes)" not in prompt


def test_generate_release_draft_names_an_empty_docs_diff(tmp_path: Path) -> None:
    changes = _changes(docs=NO_DIFF)

    prompt = _drafting_prompt(changes, tmp_path)
    assert prompt.count("(no user documentation changes)") == 2
    assert "(no code changes)" not in prompt


def test_generate_release_draft_pins_hubs_only_without_code_or_docs_changes(
    tmp_path: Path,
) -> None:
    # Only paths the diffs leave out changed (CI, lock files, peppy's own
    # tests, the notes of the previous release): nothing is left to judge.
    changes = _changes(
        subjects=("docs: add release notes for v0.11.1", "ci: bump an action"),
        code=NO_DIFF,
        docs=NO_DIFF,
    )
    with patch("functions.release_summary.run_claude") as run_claude:
        result = generate_release_draft(changes, tmp_path)

    # Nothing changed for users, so nothing breaks: the next patch version.
    assert result == _hub_pins_only_release(NEXT_PATCH)
    run_claude.assert_not_called()


def test_generate_release_draft_pins_hubs_only_when_claude_finds_no_user_facing_change(
    tmp_path: Path,
) -> None:
    with patch(
        "functions.release_summary.run_claude", return_value=NO_USER_FACING_CHANGE
    ) as run_claude:
        result = generate_release_draft(_changes(), tmp_path)

    assert result == _hub_pins_only_release(NEXT_PATCH)
    run_claude.assert_called_once()


def test_generate_release_draft_without_a_previous_release(tmp_path: Path) -> None:
    changes = ReleaseChanges(commit_subjects=("initial commit",), diffs=None)

    # The full history counts as changed: Claude judges it.
    prompt = _drafting_prompt(changes, tmp_path)
    assert "The previous release\nis none: no release has been published yet" in prompt
    assert "- initial commit" in prompt
    # Every diff section says why it is empty rather than claiming no changes.
    assert prompt.count("(no previous release to diff against)") == 4
    assert "(no code changes)" not in prompt
    assert "(no user documentation changes)" not in prompt


# --- ReleaseChanges.has_code_or_docs_changes ---


@pytest.mark.parametrize(
    ("code", "docs", "expected"),
    [
        (PathDiff("+x", ("crates/peppy/src/main.rs",)), NO_DIFF, True),
        (NO_DIFF, PathDiff("+x", ("docs/src/content/docs/x.mdx",)), True),
        (NO_DIFF, NO_DIFF, False),
    ],
)
def test_has_code_or_docs_changes_reads_the_changed_paths(
    code: PathDiff, docs: PathDiff, expected: bool
) -> None:
    assert _changes(code=code, docs=docs).has_code_or_docs_changes is expected


def test_has_code_or_docs_changes_without_a_previous_release() -> None:
    changes = ReleaseChanges(commit_subjects=("initial commit",), diffs=None)
    assert changes.has_code_or_docs_changes is True


# --- ReleaseChanges.previous ---


def test_the_changes_name_the_release_they_follow() -> None:
    assert _changes().previous == PREVIOUS
    assert ReleaseChanges(commit_subjects=(), diffs=None).previous is None


# --- collect_release_changes ---


def test_collect_release_changes_gathers_subjects_and_both_diffs(
    tmp_path: Path,
) -> None:
    with (
        patch(
            "functions.release_summary.get_commit_subjects",
            return_value=["feat: b", "fix: a"],
        ) as subjects,
        patch(
            "functions.release_summary.get_code_diff",
            return_value=("CODE", ["crates/peppy/src/main.rs"]),
        ) as code,
        patch(
            "functions.release_summary.get_docs_diff",
            return_value=("DOCS", ["docs/src/content/docs/x.mdx"]),
        ) as docs,
    ):
        changes = collect_release_changes(PREVIOUS, "abc123", tmp_path)

    assert changes == ReleaseChanges(
        commit_subjects=("feat: b", "fix: a"),
        diffs=ReleaseDiffs(
            previous=PREVIOUS,
            code=PathDiff("CODE", ("crates/peppy/src/main.rs",)),
            docs=PathDiff("DOCS", ("docs/src/content/docs/x.mdx",)),
        ),
    )
    # Everything is measured over the same range: previous tag to the exact
    # commit being released.
    subjects.assert_called_once_with("v0.11.1", "abc123")
    code.assert_called_once_with("v0.11.1", "abc123", tmp_path)
    docs.assert_called_once_with("v0.11.1", "abc123", tmp_path)


def test_collect_release_changes_skips_diffs_without_a_previous_release(
    tmp_path: Path,
) -> None:
    with (
        patch(
            "functions.release_summary.get_commit_subjects",
            return_value=["initial commit"],
        ) as subjects,
        patch("functions.release_summary.get_code_diff") as code,
        patch("functions.release_summary.get_docs_diff") as docs,
    ):
        changes = collect_release_changes(None, "abc123", tmp_path)

    assert changes == ReleaseChanges(commit_subjects=("initial commit",), diffs=None)
    # The full history is listed; there is no earlier state to diff against.
    subjects.assert_called_once_with(None, "abc123")
    code.assert_not_called()
    docs.assert_not_called()
