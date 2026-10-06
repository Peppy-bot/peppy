"""Tests for functions.release_summary module."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import patch

import pytest

from functions.cli import ReleaseError
from functions.release_summary import (
    _CONTENT_SCHEMA,
    PathDiff,
    ReleaseChanges,
    ReleaseContent,
    ReleaseDiffs,
    _parse_release_content,
    collect_release_changes,
    generate_release_content,
    hub_pins_only_content,
)

TAG = "v0.12.0"

# What Claude answers for a release with user-facing changes.
DRAFTED = {
    "has_user_facing_changes": True,
    "title": "T",
    "description": "D",
    "notes": "N",
}

# What Claude answers for a release with no user-facing change.
NO_USER_FACING_CHANGE = {
    "has_user_facing_changes": False,
    "title": "",
    "description": "",
    "notes": "",
}

NO_DIFF = PathDiff("", ())


def _changes(
    subjects: tuple[str, ...] = ("fix(apptainer): pre-flight bind mounts",),
    code: PathDiff = PathDiff("+fn main() {}", ("crates/peppy/src/main.rs",)),
    docs: PathDiff = PathDiff("+## The clock", ("docs/src/content/docs/x.mdx",)),
) -> ReleaseChanges:
    return ReleaseChanges(
        commit_subjects=subjects,
        diffs=ReleaseDiffs(previous_tag="v0.11.1", code=code, docs=docs),
    )


# --- _CONTENT_SCHEMA ---


def test_content_schema_requires_the_verdict_and_every_text_field() -> None:
    # The Python parser and the CLI-side schema must not drift.
    # The verdict comes last, after the fields it decides about.
    assert list(_CONTENT_SCHEMA["properties"]) == [
        "title",
        "description",
        "notes",
        "has_user_facing_changes",
    ]
    assert _CONTENT_SCHEMA["required"] == list(_CONTENT_SCHEMA["properties"])
    assert _CONTENT_SCHEMA["properties"]["has_user_facing_changes"] == {
        "type": "boolean"
    }
    # A text field is empty when Claude reports no user-facing change, so the
    # schema must accept an empty string.
    for field in ("title", "description", "notes"):
        assert _CONTENT_SCHEMA["properties"][field] == {"type": "string"}


# --- hub_pins_only_content ---


def test_hub_pins_only_content_names_the_hub_tag_of_the_release() -> None:
    content = hub_pins_only_content(TAG)

    assert "only pins newer commits of the default hubs" in content.description
    assert "only pins newer commits of the default hubs" in content.notes
    assert "`peppy-release/v0.12.0`" in content.notes
    # Published unreviewed, as every release body is: it follows the release
    # notes rules (a bulleted list, no em-dash).
    assert all(line.startswith("- ") for line in content.notes.splitlines())
    for text in (content.title, content.description, content.notes):
        assert text.strip()
        assert "\u2014" not in text


# --- _parse_release_content ---


def test_parse_release_content_valid() -> None:
    payload = {
        "has_user_facing_changes": True,
        "title": "Topics hardening",
        "description": "Fixed deadlocks.",
        "notes": "## What's Changed\n- x",
    }
    assert _parse_release_content(payload, TAG) == ReleaseContent(
        title="Topics hardening",
        description="Fixed deadlocks.",
        notes="## What's Changed\n- x",
    )


def test_parse_release_content_strips_whitespace() -> None:
    payload = {
        "has_user_facing_changes": True,
        "title": "  T  ",
        "description": " D ",
        "notes": " N ",
    }
    assert _parse_release_content(payload, TAG) == ReleaseContent("T", "D", "N")


@pytest.mark.parametrize("missing", ["title", "description", "notes"])
def test_parse_release_content_missing_field(missing: str) -> None:
    payload = dict(DRAFTED)
    del payload[missing]
    with pytest.raises(ReleaseError, match=f"missing non-empty '{missing}'"):
        _parse_release_content(payload, TAG)


@pytest.mark.parametrize("field", ["title", "description", "notes"])
def test_parse_release_content_blank_field(field: str) -> None:
    # The schema accepts an empty string, since a release with no user-facing
    # change leaves every text field empty; the Python-side check refuses a
    # blank field of a release with user-facing changes.
    payload = dict(DRAFTED)
    payload[field] = "   "
    with pytest.raises(ReleaseError, match=f"missing non-empty '{field}'"):
        _parse_release_content(payload, TAG)


def test_parse_release_content_non_string_field() -> None:
    payload: dict[str, object] = {**DRAFTED, "notes": 123}
    with pytest.raises(ReleaseError, match="missing non-empty 'notes'"):
        _parse_release_content(payload, TAG)


def test_parse_release_content_without_user_facing_changes_pins_hubs_only() -> None:
    assert _parse_release_content(NO_USER_FACING_CHANGE, TAG) == (
        hub_pins_only_content(TAG)
    )


def test_parse_release_content_takes_whitespace_as_an_empty_field() -> None:
    payload = {**NO_USER_FACING_CHANGE, "notes": " \n "}
    assert _parse_release_content(payload, TAG) == hub_pins_only_content(TAG)


@pytest.mark.parametrize("field", ["title", "description", "notes"])
def test_parse_release_content_refuses_text_without_user_facing_changes(
    field: str,
) -> None:
    # A contradictory answer must not decide what the unreviewed notes say.
    payload = {**NO_USER_FACING_CHANGE, field: "Internal improvements."}
    with pytest.raises(
        ReleaseError,
        match=f"no user-facing change but did not leave '{field}' empty",
    ):
        _parse_release_content(payload, TAG)


@pytest.mark.parametrize("field", ["title", "description", "notes"])
def test_parse_release_content_refuses_a_missing_field_without_user_facing_changes(
    field: str,
) -> None:
    payload = dict(NO_USER_FACING_CHANGE)
    del payload[field]
    with pytest.raises(
        ReleaseError,
        match=f"no user-facing change but did not leave '{field}' empty",
    ):
        _parse_release_content(payload, TAG)


@pytest.mark.parametrize("verdict", [None, "false", 0])
def test_parse_release_content_requires_a_boolean_verdict(verdict: object) -> None:
    payload: dict[str, object] = {**DRAFTED, "has_user_facing_changes": verdict}
    if verdict is None:
        del payload["has_user_facing_changes"]
    with pytest.raises(
        ReleaseError, match="missing boolean 'has_user_facing_changes'"
    ):
        _parse_release_content(payload, TAG)


# --- generate_release_content (mocked run_claude) ---


def _capture_prompt(captured: dict[str, object]) -> object:
    def _fake_run_claude(prompt: str, **kwargs: object) -> dict:
        captured["prompt"] = prompt
        return DRAFTED

    return _fake_run_claude


def test_generate_release_content_runs_claude_without_tools(tmp_path: Path) -> None:
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
        result = generate_release_content(changes, "v0.12.0", tmp_path)

    assert result == ReleaseContent("T", "D", "N")
    # Pure transformation: tools disabled so Claude cannot explore and ramble.
    assert captured["tools"] == ""
    assert captured["allowed_tools"] == ""
    assert captured["effort"] == "xhigh"
    assert captured["cwd"] == tmp_path
    # The heartbeat names the work while Claude drafts.
    assert captured["activity"] == "drafting the release notes"
    # The response shape is enforced CLI-side.
    assert captured["json_schema"] == _CONTENT_SCHEMA
    # The tag, the previous tag, and the commit subjects are interpolated.
    prompt = captured["prompt"]
    assert isinstance(prompt, str)
    assert "release\nv0.12.0; the previous release is v0.11.1." in prompt
    assert "- fix(apptainer): pre-flight bind mounts\n- refactor: extract helper" in prompt
    # Claude can report that no change is user-facing instead of drafting.
    assert '"has_user_facing_changes": true when the notes list' in prompt


def test_generate_release_content_feeds_both_diffs_and_their_paths(
    tmp_path: Path,
) -> None:
    captured: dict[str, object] = {}
    changes = _changes(
        code=PathDiff(
            "+fn main() {}",
            ("crates/peppy/src/main.rs", "crates/peppy/src/cli.rs"),
        ),
        docs=PathDiff("+## The clock", ("docs/src/content/docs/x.mdx",)),
    )
    with patch("functions.release_summary.run_claude", side_effect=_capture_prompt(captured)):
        generate_release_content(changes, "v0.12.0", tmp_path)

    prompt = captured["prompt"]
    assert isinstance(prompt, str)
    assert (
        "Changed code paths:\ncrates/peppy/src/main.rs\ncrates/peppy/src/cli.rs\n"
        in prompt
    )
    assert "Code diff:\n+fn main() {}\n" in prompt
    assert "Changed user documentation paths:\ndocs/src/content/docs/x.mdx\n" in prompt
    assert "User documentation diff:\n+## The clock\n" in prompt


def test_generate_release_content_truncates_each_diff_separately(
    tmp_path: Path,
) -> None:
    captured: dict[str, object] = {}
    changes = _changes(
        code=PathDiff("c" * 500_000, ("crates/peppy/src/main.rs",)),
        docs=PathDiff("d" * 1000, ("docs/src/content/docs/x.mdx",)),
    )
    with patch("functions.release_summary.run_claude", side_effect=_capture_prompt(captured)):
        generate_release_content(changes, "v0.12.0", tmp_path)

    prompt = captured["prompt"]
    assert isinstance(prompt, str)
    # The oversized code diff is cut, and the docs diff survives untouched.
    assert prompt.count("diff truncated") == 1
    assert "d" * 1000 in prompt
    assert "c" * 500_000 not in prompt


def test_generate_release_content_names_an_empty_code_diff(tmp_path: Path) -> None:
    captured: dict[str, object] = {}
    changes = _changes(subjects=(), code=NO_DIFF)
    with patch("functions.release_summary.run_claude", side_effect=_capture_prompt(captured)):
        generate_release_content(changes, "v0.12.0", tmp_path)

    prompt = captured["prompt"]
    assert isinstance(prompt, str)
    assert "(no commits since the last release)" in prompt
    assert prompt.count("(no code changes)") == 2
    assert "(no user documentation changes)" not in prompt


def test_generate_release_content_names_an_empty_docs_diff(tmp_path: Path) -> None:
    captured: dict[str, object] = {}
    changes = _changes(docs=NO_DIFF)
    with patch("functions.release_summary.run_claude", side_effect=_capture_prompt(captured)):
        generate_release_content(changes, "v0.12.0", tmp_path)

    prompt = captured["prompt"]
    assert isinstance(prompt, str)
    assert prompt.count("(no user documentation changes)") == 2
    assert "(no code changes)" not in prompt


def test_generate_release_content_pins_hubs_only_without_code_or_docs_changes(
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
        result = generate_release_content(changes, "v0.12.0", tmp_path)

    assert result == hub_pins_only_content("v0.12.0")
    run_claude.assert_not_called()


def test_generate_release_content_pins_hubs_only_when_claude_finds_no_user_facing_change(
    tmp_path: Path,
) -> None:
    with patch(
        "functions.release_summary.run_claude", return_value=NO_USER_FACING_CHANGE
    ) as run_claude:
        result = generate_release_content(_changes(), "v0.12.0", tmp_path)

    assert result == hub_pins_only_content("v0.12.0")
    run_claude.assert_called_once()


def test_generate_release_content_without_a_previous_release(tmp_path: Path) -> None:
    captured: dict[str, object] = {}
    changes = ReleaseChanges(commit_subjects=("initial commit",), diffs=None)
    with patch("functions.release_summary.run_claude", side_effect=_capture_prompt(captured)):
        generate_release_content(changes, "v0.1.0", tmp_path)

    # The full history counts as changed: Claude judges it.
    prompt = captured["prompt"]
    assert isinstance(prompt, str)
    assert "the previous release is none: no release has been published yet" in prompt
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


# --- collect_release_changes ---


def test_collect_release_changes_gathers_subjects_and_both_diffs(
    tmp_path: Path,
) -> None:
    with patch(
        "functions.release_summary.get_commit_subjects",
        return_value=["feat: b", "fix: a"],
    ) as subjects, patch(
        "functions.release_summary.get_code_diff",
        return_value=("CODE", ["crates/peppy/src/main.rs"]),
    ) as code, patch(
        "functions.release_summary.get_docs_diff",
        return_value=("DOCS", ["docs/src/content/docs/x.mdx"]),
    ) as docs:
        changes = collect_release_changes("v0.11.1", "abc123", tmp_path)

    assert changes == ReleaseChanges(
        commit_subjects=("feat: b", "fix: a"),
        diffs=ReleaseDiffs(
            previous_tag="v0.11.1",
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
    with patch(
        "functions.release_summary.get_commit_subjects",
        return_value=["initial commit"],
    ) as subjects, patch(
        "functions.release_summary.get_code_diff"
    ) as code, patch(
        "functions.release_summary.get_docs_diff"
    ) as docs:
        changes = collect_release_changes(None, "abc123", tmp_path)

    assert changes == ReleaseChanges(commit_subjects=("initial commit",), diffs=None)
    # The full history is listed; there is no earlier state to diff against.
    subjects.assert_called_once_with(None, "abc123")
    code.assert_not_called()
    docs.assert_not_called()
