"""Tests for functions.release_summary module."""

from __future__ import annotations

import json
from pathlib import Path
from unittest.mock import patch

import pytest

from functions.cli import ReleaseError
from functions.release_summary import (
    ReleaseContent,
    _parse_release_content,
    generate_release_content,
)


# --- _parse_release_content ---


def test_parse_release_content_valid() -> None:
    text = json.dumps(
        {
            "title": "Topics hardening",
            "description": "Fixed deadlocks.",
            "notes": "## What's Changed\n- x",
        }
    )
    assert _parse_release_content(text) == ReleaseContent(
        title="Topics hardening",
        description="Fixed deadlocks.",
        notes="## What's Changed\n- x",
    )


def test_parse_release_content_strips_code_fence() -> None:
    text = '```json\n{"title": "T", "description": "D", "notes": "N"}\n```'
    assert _parse_release_content(text) == ReleaseContent("T", "D", "N")


def test_parse_release_content_strips_whitespace() -> None:
    payload = {"title": "  T  ", "description": " D ", "notes": " N "}
    assert _parse_release_content(json.dumps(payload)) == ReleaseContent("T", "D", "N")


def test_parse_release_content_invalid_json() -> None:
    with pytest.raises(ReleaseError, match="not valid JSON"):
        _parse_release_content("not json at all")


def test_parse_release_content_not_object() -> None:
    with pytest.raises(ReleaseError, match="must be a JSON object"):
        _parse_release_content("[]")


@pytest.mark.parametrize("missing", ["title", "description", "notes"])
def test_parse_release_content_missing_field(missing: str) -> None:
    payload = {"title": "T", "description": "D", "notes": "N"}
    del payload[missing]
    with pytest.raises(ReleaseError, match=f"missing non-empty '{missing}'"):
        _parse_release_content(json.dumps(payload))


@pytest.mark.parametrize("field", ["title", "description", "notes"])
def test_parse_release_content_blank_field(field: str) -> None:
    payload = {"title": "T", "description": "D", "notes": "N"}
    payload[field] = "   "
    with pytest.raises(ReleaseError, match=f"missing non-empty '{field}'"):
        _parse_release_content(json.dumps(payload))


def test_parse_release_content_non_string_field() -> None:
    payload: dict[str, object] = {"title": "T", "description": "D", "notes": 123}
    with pytest.raises(ReleaseError, match="missing non-empty 'notes'"):
        _parse_release_content(json.dumps(payload))


def test_parse_release_content_tolerates_preamble() -> None:
    # Claude occasionally prefixes the object with a sentence; extract anyway.
    text = 'Here are the notes:\n{"title": "T", "description": "D", "notes": "N"}'
    assert _parse_release_content(text) == ReleaseContent("T", "D", "N")


def test_parse_release_content_extracts_object_with_braces_in_notes() -> None:
    # Braces inside the notes string must not unbalance the extraction.
    notes = "Use `cfg{}` blocks"
    text = "noise " + json.dumps(
        {"title": "T", "description": "D", "notes": notes}
    )
    assert _parse_release_content(text) == ReleaseContent("T", "D", notes)


# --- generate_release_content (mocked run_claude) ---


def test_generate_release_content_runs_claude_without_tools(tmp_path: Path) -> None:
    captured: dict[str, object] = {}

    def _fake_run_claude(
        prompt: str,
        *,
        allowed_tools: str,
        permission_mode: str,
        cwd: Path,
        tools: str | None = None,
    ) -> str:
        captured["prompt"] = prompt
        captured["allowed_tools"] = allowed_tools
        captured["permission_mode"] = permission_mode
        captured["cwd"] = cwd
        captured["tools"] = tools
        return json.dumps({"title": "T", "description": "D", "notes": "N"})

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
    assert captured["cwd"] == tmp_path
    # The commit subjects and tag are interpolated into the prompt.
    assert "- fix(apptainer): pre-flight bind mounts" in captured["prompt"]
    assert "v0.12.0" in captured["prompt"]


def test_generate_release_content_handles_no_commits(tmp_path: Path) -> None:
    captured: dict[str, str] = {}

    def _fake_run_claude(prompt: str, **kwargs: object) -> str:
        captured["prompt"] = prompt
        return json.dumps({"title": "T", "description": "D", "notes": "N"})

    with patch("functions.release_summary.run_claude", side_effect=_fake_run_claude):
        generate_release_content([], "v0.1.0", tmp_path)

    assert "no commits since the last release" in captured["prompt"]
