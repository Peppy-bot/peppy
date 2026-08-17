"""Tests for functions.release_summary module."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import patch

import pytest

from functions.cli import ReleaseError
from functions.release_summary import (
    _CONTENT_SCHEMA,
    ReleaseContent,
    _parse_release_content,
    generate_release_content,
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

    with patch("functions.release_summary.run_claude", side_effect=_fake_run_claude):
        result = generate_release_content(
            ["fix(apptainer): pre-flight bind mounts", "refactor: extract helper"],
            "v0.12.0",
            tmp_path,
        )

    assert result == ReleaseContent("T", "D", "N")
    # Pure transformation: tools disabled so Claude cannot explore and ramble.
    assert captured["tools"] == ""
    assert captured["allowed_tools"] == ""
    assert captured["effort"] == "xhigh"
    assert captured["cwd"] == tmp_path
    # The response shape is enforced CLI-side.
    assert captured["json_schema"] == _CONTENT_SCHEMA
    # The commit subjects and tag are interpolated into the prompt.
    assert "- fix(apptainer): pre-flight bind mounts" in captured["prompt"]
    assert "v0.12.0" in captured["prompt"]


def test_generate_release_content_handles_no_commits(tmp_path: Path) -> None:
    captured: dict[str, str] = {}

    def _fake_run_claude(prompt: str, **kwargs: object) -> dict:
        captured["prompt"] = prompt
        return {"title": "T", "description": "D", "notes": "N"}

    with patch("functions.release_summary.run_claude", side_effect=_fake_run_claude):
        generate_release_content([], "v0.1.0", tmp_path)

    assert "no commits since the last release" in captured["prompt"]
