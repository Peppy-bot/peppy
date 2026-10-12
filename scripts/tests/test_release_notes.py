"""Tests for functions.release_notes."""

from __future__ import annotations

import datetime
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import httpx
import pytest
import respx

from functions.github import RepoSlug
from functions.release_notes import (
    ReleaseNotesInput,
    extract_release_date,
    fetch_release_body_html,
    generate_release_notes_file,
    release_notes_file,
    release_page_path,
)

SLUG = RepoSlug(owner="test-owner", repo="test-repo")
RELEASE_URL = f"{SLUG.api_url}/releases/42"
HTML_ACCEPT = "application/vnd.github.v3.html+json"


def test_release_notes_file_is_named_after_the_tag() -> None:
    assert release_notes_file("v0.3.0") == Path("docs/src/content/releases/v0.3.0.html")


def test_release_page_path_is_the_slug_the_docs_site_gives_the_release() -> None:
    # releaseSlugFromVersion in docs/src/lib/releaseEntry.js.
    assert release_page_path("v0.31.12") == "/releases/v0-31-12/"


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


@pytest.mark.parametrize(
    ("details", "expected"),
    [
        pytest.param(
            {
                "published_at": "2026-09-25T10:00:00Z",
                "created_at": "2026-09-24T09:00:00Z",
            },
            datetime.date(2026, 9, 25),
            id="published",
        ),
        # A draft has no publication date yet.
        pytest.param(
            {"published_at": None, "created_at": "2026-09-24T09:00:00Z"},
            datetime.date(2026, 9, 24),
            id="created",
        ),
        pytest.param(
            {"published_at": "not a date", "created_at": "2026-09-24T09:00:00Z"},
            datetime.date(2026, 9, 24),
            id="unreadable-published",
        ),
    ],
)
def test_extract_release_date_reads_the_first_usable_date(
    details: dict[str, object], expected: datetime.date
) -> None:
    assert extract_release_date(details) == expected


class _FrozenDate(datetime.date):
    @classmethod
    def today(cls) -> _FrozenDate:
        return cls(2026, 10, 10)


class _FrozenDateTime(datetime.datetime):
    @classmethod
    def now(cls, tz: datetime.tzinfo | None = None) -> _FrozenDateTime:
        return cls(2026, 10, 10, 12, 0, tzinfo=tz)


def test_extract_release_date_falls_back_to_the_local_date_of_today() -> None:
    # The clock the module reads is frozen, so the date of today is known.
    frozen = SimpleNamespace(date=_FrozenDate, datetime=_FrozenDateTime)
    with patch("functions.release_notes.datetime", frozen):
        found = extract_release_date({"published_at": None, "created_at": "nope"})

    assert found == datetime.date(2026, 10, 10)
