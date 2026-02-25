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


class TestExtractReleaseDate:
    def test_published_at(self) -> None:
        details = {"published_at": "2025-06-15T10:00:00Z", "created_at": "2025-06-14T09:00:00Z"}
        result = extract_release_date(details)
        assert result == datetime.date(2025, 6, 15)

    def test_created_at_fallback(self) -> None:
        details = {"created_at": "2025-03-20T12:00:00Z"}
        result = extract_release_date(details)
        assert result == datetime.date(2025, 3, 20)

    def test_fallback_today(self) -> None:
        result = extract_release_date({})
        assert result == datetime.date.today()

    def test_invalid_date_falls_back(self) -> None:
        details = {"published_at": "not-a-date", "created_at": "also-not"}
        result = extract_release_date(details)
        assert result == datetime.date.today()

    def test_short_string_skipped(self) -> None:
        details = {"published_at": "short"}
        result = extract_release_date(details)
        assert result == datetime.date.today()


class TestBuildArticleHtml:
    def test_basic(self) -> None:
        result = build_article_html("v0.1.0", "First release", "June 15, 2025", "")
        assert "<h1>v0.1.0</h1>" in result
        assert "<em>First release</em>" in result
        assert "June 15, 2025" in result
        assert "<article>" in result
        assert "</article>" in result

    def test_with_body(self) -> None:
        result = build_article_html("v0.1.0", "Release", "June 15, 2025", "<p>Changes here</p>")
        assert "<p>Changes here</p>" in result

    def test_html_escaping(self) -> None:
        result = build_article_html("v0.1.0", "Release <script>", "June 15, 2025", "")
        assert "<script>" not in result
        assert "&lt;script&gt;" in result

    def test_empty_body_excluded(self) -> None:
        result = build_article_html("v0.1.0", "Release", "June 15, 2025", "")
        lines = result.split("\n")
        # Body line should not be present between </header> and </article>
        header_end = next(i for i, line in enumerate(lines) if "</header>" in line)
        article_end = next(i for i, line in enumerate(lines) if "</article>" in line)
        assert article_end == header_end + 1


class TestBuildAtomEntry:
    def test_basic(self) -> None:
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

    def test_html_escaping_in_content(self) -> None:
        result = build_atom_entry(
            "v0.1.0",
            "Release",
            "2025-06-15T00:00:00Z",
            "https://example.com",
            "<article><h1>Test</h1></article>",
        )
        # The article HTML should be escaped inside the <content> element
        assert "&lt;article&gt;" in result


class TestNormalizeTag:
    """Test tag normalization through generate_release_notes_file."""

    def test_tag_with_v(self, tmp_path: Path) -> None:
        notes_input = ReleaseNotesInput(
            tag="v0.1.0",
            description="Test",
            release_details={"published_at": "2025-06-15T00:00:00Z"},
            body_html="",
        )
        with patch("functions.release_notes.prompt_yn", return_value=True):
            result = generate_release_notes_file(
                notes_input, tmp_path, confirm_overwrite=False
            )
        assert result.name == "v0.1.0.html"

    def test_tag_without_v(self, tmp_path: Path) -> None:
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

    def test_tag_with_uppercase_v(self, tmp_path: Path) -> None:
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


class TestGenerateReleaseNotesFile:
    def test_creates_file(self, tmp_path: Path) -> None:
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

    def test_overwrite_prompt_declined(self, tmp_path: Path) -> None:
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

    def test_overwrite_prompt_accepted(self, tmp_path: Path) -> None:
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

    def test_golden_file(self, tmp_path: Path) -> None:
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


class TestFetchReleaseBodyHtml:
    def _mock_release_api(
        self,
        mock_api: respx.MockRouter,
        json_body: dict,
        html_body: dict,
    ) -> None:
        """Set up mocks for the two GET requests to the release URL.

        The function calls the same URL twice with different Accept headers:
        1. Default accept → JSON body (markdown fallback)
        2. HTML accept → HTML body
        """
        release_url = "https://api.github.com/repos/owner/repo/releases/1"

        def route_handler(request: httpx.Request) -> httpx.Response:
            accept = request.headers.get("accept", "")
            if "html" in accept:
                return httpx.Response(200, json=html_body)
            return httpx.Response(200, json=json_body)

        mock_api.get(release_url).mock(side_effect=route_handler)

    def test_html_body(
        self,
        github_client: httpx.Client,
        mock_api: respx.MockRouter,
    ) -> None:
        self._mock_release_api(
            mock_api,
            json_body={"body": "# Markdown"},
            html_body={"body_html": "<h1>Markdown</h1>"},
        )
        slug = RepoSlug(owner="owner", repo="repo")
        result = fetch_release_body_html(github_client, 1, slug)
        assert "<h1>Markdown</h1>" in result

    def test_markdown_fallback(
        self,
        github_client: httpx.Client,
        mock_api: respx.MockRouter,
    ) -> None:
        self._mock_release_api(
            mock_api,
            json_body={"body": "# Some Notes"},
            html_body={"body_html": ""},
        )
        slug = RepoSlug(owner="owner", repo="repo")
        result = fetch_release_body_html(github_client, 1, slug)
        assert "<pre><code>" in result
        assert "# Some Notes" in result

    def test_empty_body(
        self,
        github_client: httpx.Client,
        mock_api: respx.MockRouter,
    ) -> None:
        self._mock_release_api(
            mock_api,
            json_body={"body": ""},
            html_body={"body_html": ""},
        )
        slug = RepoSlug(owner="owner", repo="repo")
        result = fetch_release_body_html(github_client, 1, slug)
        assert result == ""
