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
)


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


def test_content_schema_requires_all_fields_non_empty() -> None:
    # The Python validator and the CLI-side schema must not drift.
    assert _CONTENT_SCHEMA["required"] == ["title", "description", "notes"]
    for field in ("title", "description", "notes"):
        assert _CONTENT_SCHEMA["properties"][field]["minLength"] == 1


# --- _parse_release_content ---


def test_parse_release_content_valid() -> None:
    payload = {
        "title": "Topics hardening",
        "description": "Fixed deadlocks.",
        "notes": "## What's Changed\n- x",
    }
    assert _parse_release_content(payload) == ReleaseContent(
        title="Topics hardening",
        description="Fixed deadlocks.",
        notes="## What's Changed\n- x",
    )


def test_parse_release_content_strips_whitespace() -> None:
    payload = {"title": "  T  ", "description": " D ", "notes": " N "}
    assert _parse_release_content(payload) == ReleaseContent("T", "D", "N")


@pytest.mark.parametrize("missing", ["title", "description", "notes"])
def test_parse_release_content_missing_field(missing: str) -> None:
    payload = {"title": "T", "description": "D", "notes": "N"}
    del payload[missing]
    with pytest.raises(ReleaseError, match=f"missing non-empty '{missing}'"):
        _parse_release_content(payload)


@pytest.mark.parametrize("field", ["title", "description", "notes"])
def test_parse_release_content_blank_field(field: str) -> None:
    # minLength in the schema cannot catch whitespace-only strings; the
    # Python-side check must.
    payload = {"title": "T", "description": "D", "notes": "N"}
    payload[field] = "   "
    with pytest.raises(ReleaseError, match=f"missing non-empty '{field}'"):
        _parse_release_content(payload)


def test_parse_release_content_non_string_field() -> None:
    payload: dict[str, object] = {"title": "T", "description": "D", "notes": 123}
    with pytest.raises(ReleaseError, match="missing non-empty 'notes'"):
        _parse_release_content(payload)


# --- generate_release_content (mocked run_claude) ---


def _capture_prompt(captured: dict[str, object]) -> object:
    def _fake_run_claude(prompt: str, **kwargs: object) -> dict:
        captured["prompt"] = prompt
        return {"title": "T", "description": "D", "notes": "N"}

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
        tools: str | None = None,
        effort: str = "max",
    ) -> dict:
        captured["prompt"] = prompt
        captured["allowed_tools"] = allowed_tools
        captured["permission_mode"] = permission_mode
        captured["cwd"] = cwd
        captured["json_schema"] = json_schema
        captured["tools"] = tools
        captured["effort"] = effort
        return {"title": "T", "description": "D", "notes": "N"}

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
    # The response shape is enforced CLI-side.
    assert captured["json_schema"] == _CONTENT_SCHEMA
    # The tag, the previous tag, and the commit subjects are interpolated.
    prompt = captured["prompt"]
    assert isinstance(prompt, str)
    assert "release\nv0.12.0; the previous release is v0.11.1." in prompt
    assert "- fix(apptainer): pre-flight bind mounts\n- refactor: extract helper" in prompt


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


def test_generate_release_content_names_empty_sections(tmp_path: Path) -> None:
    captured: dict[str, object] = {}
    changes = _changes(
        subjects=(),
        code=PathDiff("", ()),
        docs=PathDiff("", ()),
    )
    with patch("functions.release_summary.run_claude", side_effect=_capture_prompt(captured)):
        generate_release_content(changes, "v0.12.0", tmp_path)

    prompt = captured["prompt"]
    assert isinstance(prompt, str)
    assert "(no commits since the last release)" in prompt
    assert prompt.count("(no code changes)") == 2
    assert prompt.count("(no user documentation changes)") == 2


def test_generate_release_content_without_a_previous_release(tmp_path: Path) -> None:
    captured: dict[str, object] = {}
    changes = ReleaseChanges(commit_subjects=("initial commit",), diffs=None)
    with patch("functions.release_summary.run_claude", side_effect=_capture_prompt(captured)):
        generate_release_content(changes, "v0.1.0", tmp_path)

    prompt = captured["prompt"]
    assert isinstance(prompt, str)
    assert "the previous release is none: no release has been published yet" in prompt
    assert "- initial commit" in prompt
    # Every diff section says why it is empty rather than claiming no changes.
    assert prompt.count("(no previous release to diff against)") == 4
    assert "(no code changes)" not in prompt
    assert "(no user documentation changes)" not in prompt


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
