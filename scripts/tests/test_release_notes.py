"""Tests for functions.release_notes."""

from __future__ import annotations

from pathlib import Path

import httpx
import pytest
import respx

from functions.github import RepoSlug
from functions.release_notes import (
    ReleaseNotesInput,
    fetch_release_body_html,
    generate_release_notes_file,
    release_notes_file,
)

SLUG = RepoSlug(owner="test-owner", repo="test-repo")
RELEASE_URL = f"{SLUG.api_url}/releases/42"
HTML_ACCEPT = "application/vnd.github.v3.html+json"


def test_release_notes_file_is_named_after_the_tag() -> None:
    assert release_notes_file("v0.3.0") == Path("docs/src/content/releases/v0.3.0.html")


def _notes_input(description: str) -> ReleaseNotesInput:
    return ReleaseNotesInput(
        tag="v0.3.0",
        description=description,
        release_details={"published_at": "2026-09-25T10:00:00Z"},
        body_html="<ul><li>a change</li></ul>",
    )


def test_generate_release_notes_file_writes_the_file_of_the_tag(tmp_path: Path) -> None:
    written = generate_release_notes_file(_notes_input("Hardened the API."), tmp_path)

    assert written == tmp_path / release_notes_file("v0.3.0")
    text = written.read_text(encoding="utf-8")
    assert "Hardened the API." in text
    assert "Released on September 25, 2026" in text
    assert "https://docs.peppy.bot/releases/v0-3-0/" in text


def test_generate_release_notes_file_writes_over_the_file_it_finds(
    tmp_path: Path,
) -> None:
    # The release is the only source of its notes, and no stage asks a
    # question, so a file already there is replaced.
    generate_release_notes_file(_notes_input("First draft."), tmp_path)

    written = generate_release_notes_file(_notes_input("As published."), tmp_path)

    text = written.read_text(encoding="utf-8")
    assert "As published." in text
    assert "First draft." not in text


def _answer_release(mock_api: respx.MockRouter, *, body_html: str) -> None:
    """Make the release answer *body_html* in its HTML representation."""
    mock_api.get(RELEASE_URL, headers={"Accept": HTML_ACCEPT}).mock(
        return_value=httpx.Response(200, json={"id": 42, "body_html": body_html})
    )


def test_fetch_release_body_html_returns_the_html_github_renders(
    mock_api: respx.MockRouter, github_client: httpx.Client
) -> None:
    _answer_release(mock_api, body_html="<ul><li>a change</li></ul>")

    body_html = fetch_release_body_html(
        github_client, 42, SLUG, markdown_body="- a change"
    )

    assert body_html == "<ul><li>a change</li></ul>"


@pytest.mark.parametrize(
    ("body", "expected"),
    [
        ("a <change> & more", "<pre><code>a &lt;change&gt; &amp; more</code></pre>"),
        ("", ""),
    ],
    ids=["escaped-markdown", "empty-markdown"],
)
def test_fetch_release_body_html_falls_back_to_the_markdown_body(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
    body: str,
    expected: str,
) -> None:
    _answer_release(mock_api, body_html="")

    body_html = fetch_release_body_html(github_client, 42, SLUG, markdown_body=body)

    assert body_html == expected
