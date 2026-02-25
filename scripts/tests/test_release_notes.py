"""Tests for functions.release_notes module."""

from __future__ import annotations

import datetime
from pathlib import Path
from unittest.mock import patch

import httpx
import pytest
import respx

from functions.cli import ReleaseError
from functions.github import RepoSlug
from functions.release_notes import (
    ReleaseNotesInput,
    build_article_html,
    build_atom_entry,
    extract_release_date,
    fetch_release_body_html,
    generate_release_notes_file,
)


def test_extract_release_date_published_at() -> None:
    details = {"published_at": "2025-06-15T10:00:00Z", "created_at": "2025-06-14T09:00:00Z"}
    result = extract_release_date(details)
    assert result == datetime.date(2025, 6, 15)


def test_extract_release_date_created_at_fallback() -> None:
    details = {"created_at": "2025-03-20T12:00:00Z"}
    result = extract_release_date(details)
    assert result == datetime.date(2025, 3, 20)


def test_extract_release_date_fallback_today() -> None:
    result = extract_release_date({})
    assert result == datetime.date.today()


def test_extract_release_date_invalid_date_falls_back() -> None:
    details = {"published_at": "not-a-date", "created_at": "also-not"}
    result = extract_release_date(details)
    assert result == datetime.date.today()


def test_build_article_html_basic() -> None:
    result = build_article_html("v0.1.0", "First release", "June 15, 2025", "")
    assert "<h1>v0.1.0</h1>" in result
    assert "<em>First release</em>" in result
    assert "June 15, 2025" in result
    assert "<article>" in result
    assert "</article>" in result


def test_build_article_html_with_body() -> None:
    result = build_article_html("v0.1.0", "Release", "June 15, 2025", "<p>Changes here</p>")
    assert "<p>Changes here</p>" in result


def test_build_article_html_html_escaping() -> None:
    result = build_article_html("v0.1.0", "Release <script>", "June 15, 2025", "")
    assert "<script>" not in result
    assert "&lt;script&gt;" in result


def test_build_article_html_empty_body_excluded() -> None:
    result = build_article_html("v0.1.0", "Release", "June 15, 2025", "")
    lines = result.split("\n")
    # Body line should not be present between </header> and </article>
    header_end = next(i for i, line in enumerate(lines) if "</header>" in line)
    article_end = next(i for i, line in enumerate(lines) if "</article>" in line)
    assert article_end == header_end + 1


def test_build_atom_entry_basic() -> None:
    result = build_atom_entry(
        "v0.1.0",
        "First release",
        "2025-06-15T00:00:00Z",
        "https://docs.peppy.bot/releases/v0-1-0/",
        "<article>content</article>",
    )
    assert "<entry>" in result
    assert "</entry>" in result
    assert "<title>v0.1.0</title>" in result
    assert "<summary>First release</summary>" in result
    assert 'type="html"' in result


def test_build_atom_entry_html_escaping_in_content() -> None:
    result = build_atom_entry(
        "v0.1.0",
        "Release",
        "2025-06-15T00:00:00Z",
        "https://example.com",
        "<article><h1>Test</h1></article>",
    )
    # The article HTML should be escaped inside the <content> element
    assert "&lt;article&gt;" in result


def test_normalize_tag_tag_without_v(tmp_path: Path) -> None:
    notes_input = ReleaseNotesInput(
        tag="0.1.0",
        description="Test",
        release_details={"published_at": "2025-06-15T00:00:00Z"},
        body_html="",
    )
    result = generate_release_notes_file(
        notes_input, tmp_path, confirm_overwrite=False
    )
    assert result.name == "v0.1.0.html"


def test_normalize_tag_tag_with_uppercase_v(tmp_path: Path) -> None:
    notes_input = ReleaseNotesInput(
        tag="V0.1.0",
        description="Test",
        release_details={"published_at": "2025-06-15T00:00:00Z"},
        body_html="",
    )
    result = generate_release_notes_file(
        notes_input, tmp_path, confirm_overwrite=False
    )
    assert result.name == "V0.1.0.html"


def test_generate_release_notes_file_creates_file(tmp_path: Path) -> None:
    notes_input = ReleaseNotesInput(
        tag="v0.1.0",
        description="First release",
        release_details={"published_at": "2025-06-15T10:00:00Z"},
        body_html="<p>Release notes</p>",
    )
    result = generate_release_notes_file(
        notes_input, tmp_path, confirm_overwrite=False
    )
    assert result.exists()
    content = result.read_text()
    assert "<entry>" in content
    assert "v0.1.0" in content
    assert "First release" in content


def test_generate_release_notes_file_overwrite_prompt_declined(tmp_path: Path) -> None:
    existing = tmp_path / "v0.1.0.html"
    existing.write_text("old content")

    notes_input = ReleaseNotesInput(
        tag="v0.1.0",
        description="New release",
        release_details={},
        body_html="",
    )
    with patch("functions.release_notes.prompt_yn", return_value=False):
        with pytest.raises(ReleaseError, match="refusing to overwrite"):
            generate_release_notes_file(notes_input, tmp_path)


def test_generate_release_notes_file_overwrite_prompt_accepted(tmp_path: Path) -> None:
    existing = tmp_path / "v0.1.0.html"
    existing.write_text("old content")

    notes_input = ReleaseNotesInput(
        tag="v0.1.0",
        description="New release",
        release_details={"published_at": "2025-06-15T10:00:00Z"},
        body_html="",
    )
    with patch("functions.release_notes.prompt_yn", return_value=True):
        result = generate_release_notes_file(notes_input, tmp_path)
    assert result.read_text() != "old content"


def test_generate_release_notes_file_golden_file(tmp_path: Path) -> None:
    """Verify the full output matches the expected format."""
    notes_input = ReleaseNotesInput(
        tag="v0.2.0",
        description="Bug fixes & improvements",
        release_details={"published_at": "2025-07-01T12:00:00Z"},
        body_html="<ul><li>Fixed crash on startup</li></ul>",
    )
    result = generate_release_notes_file(
        notes_input, tmp_path, confirm_overwrite=False
    )
    content = result.read_text()

    # Verify structure
    assert content.startswith("<entry>")
    assert content.strip().endswith("</entry>")
    assert "<title>v0.2.0</title>" in content
    assert "<summary>Bug fixes &amp; improvements</summary>" in content
    assert "2025-07-01T00:00:00Z" in content
    assert "https://docs.peppy.bot/releases/v0-2-0/" in content
    assert 'type="html"' in content


def test_fetch_release_body_html_html_body(
    github_client: httpx.Client,
    mock_api: respx.MockRouter,
) -> None:
    release_url = "https://api.github.com/repos/owner/repo/releases/1"

    def route_handler(request: httpx.Request) -> httpx.Response:
        accept = request.headers.get("accept", "")
        if "html" in accept:
            return httpx.Response(200, json={"body_html": "<h1>Markdown</h1>"})
        return httpx.Response(200, json={"body": "# Markdown"})

    mock_api.get(release_url).mock(side_effect=route_handler)
    slug = RepoSlug(owner="owner", repo="repo")
    result = fetch_release_body_html(github_client, 1, slug)
    assert "<h1>Markdown</h1>" in result


def test_fetch_release_body_html_markdown_fallback(
    github_client: httpx.Client,
    mock_api: respx.MockRouter,
) -> None:
    release_url = "https://api.github.com/repos/owner/repo/releases/1"

    def route_handler(request: httpx.Request) -> httpx.Response:
        accept = request.headers.get("accept", "")
        if "html" in accept:
            return httpx.Response(200, json={"body_html": ""})
        return httpx.Response(200, json={"body": "# Some Notes"})

    mock_api.get(release_url).mock(side_effect=route_handler)
    slug = RepoSlug(owner="owner", repo="repo")
    result = fetch_release_body_html(github_client, 1, slug)
    assert "<pre><code>" in result
    assert "# Some Notes" in result


def test_fetch_release_body_html_empty_body(
    github_client: httpx.Client,
    mock_api: respx.MockRouter,
) -> None:
    release_url = "https://api.github.com/repos/owner/repo/releases/1"

    def route_handler(request: httpx.Request) -> httpx.Response:
        accept = request.headers.get("accept", "")
        if "html" in accept:
            return httpx.Response(200, json={"body_html": ""})
        return httpx.Response(200, json={"body": ""})

    mock_api.get(release_url).mock(side_effect=route_handler)
    slug = RepoSlug(owner="owner", repo="repo")
    result = fetch_release_body_html(github_client, 1, slug)
    assert result == ""
