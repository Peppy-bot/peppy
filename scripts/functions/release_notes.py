"""Generate HTML/Atom release note entries for the docs changelog page."""

from __future__ import annotations

import datetime
import html
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import httpx

from .cli import ReleaseError, console, prompt_yn
from .github import RepoSlug, github_api

_MONTH_NAMES = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
]


@dataclass(frozen=True)
class ReleaseNotesInput:
    """All data needed to generate a release notes file."""

    tag: str
    description: str
    release_details: dict[str, Any]
    body_html: str


def extract_release_date(details: dict[str, Any]) -> datetime.date:
    """Extract the release date from the GitHub release details JSON.

    Uses published_at or created_at, falling back to today's date.
    """
    for field in ("published_at", "created_at"):
        dt = details.get(field)
        if isinstance(dt, str) and len(dt) >= 10:
            try:
                return datetime.date.fromisoformat(dt[:10])
            except ValueError:
                continue
    return datetime.date.today()


def build_article_html(
    tag_title: str,
    description: str,
    date_text: str,
    body_html: str,
) -> str:
    """Build the <article> HTML snippet for the release notes entry.

    Escapes user-supplied text (tag_title, description, date_text) for HTML safety.
    The body_html is inserted raw (it comes pre-rendered from GitHub's API).
    """
    parts = [
        "<article>",
        "  <header>",
        f"    <h1>{html.escape(tag_title, quote=False)}</h1>",
        f"    <p><em>{html.escape(description, quote=False)}</em></p>",
        "    <p><small>",
        f"      Released on {html.escape(date_text, quote=False)}",
        "    </small></p>",
        "  </header>",
    ]
    if body_html:
        parts.append(body_html)
    parts.append("</article>")
    return "\n".join(parts)


def build_atom_entry(
    tag_title: str,
    description: str,
    updated_iso: str,
    entry_id: str,
    article_html: str,
) -> str:
    """Build the Atom <entry> XML snippet.

    HTML-escapes all content for safe embedding in the XML content element.
    """
    return "\n".join(
        [
            "<entry>",
            f"  <title>{html.escape(tag_title, quote=False)}</title>",
            f"  <id>{html.escape(entry_id, quote=False)}</id>",
            f"  <updated>{html.escape(updated_iso, quote=False)}</updated>",
            "",
            f"  <summary>{html.escape(description, quote=False)}</summary>",
            "",
            f'  <content type="html">{html.escape(article_html, quote=False)}</content>',
            "</entry>",
            "",
        ]
    )


def _normalize_tag(tag: str) -> tuple[str, str]:
    """Normalize a tag to ensure it has a 'v' prefix.

    Returns (tag_title, version) where tag_title always starts with 'v'.
    """
    tag = tag.strip()
    if tag.lower().startswith("v"):
        version = tag[1:]
        tag_title = f"v{version}"
    else:
        version = tag
        tag_title = f"v{version}"
    return tag_title, version


def generate_release_notes_file(
    notes_input: ReleaseNotesInput,
    releases_dir: Path,
    *,
    confirm_overwrite: bool = True,
) -> Path:
    """Generate and write the release notes HTML/Atom entry file.

    The output file is named {releases_dir}/v{version}.html.
    If the file already exists and confirm_overwrite is True, prompts the user.

    Returns the path to the written file.
    """
    tag_title, version = _normalize_tag(notes_input.tag)

    release_date = extract_release_date(notes_input.release_details)
    date_text = f"{_MONTH_NAMES[release_date.month - 1]} {release_date.day}, {release_date.year}"

    updated_dt = datetime.datetime(
        release_date.year,
        release_date.month,
        release_date.day,
        tzinfo=datetime.timezone.utc,
    )
    updated_iso = updated_dt.isoformat().replace("+00:00", "Z")

    docs_url = f"https://docs.peppy.bot/releases/v{version.replace('.', '-')}/"
    entry_id = docs_url

    article = build_article_html(
        tag_title, notes_input.description, date_text, notes_input.body_html
    )
    entry_xml = build_atom_entry(
        tag_title, notes_input.description, updated_iso, entry_id, article
    )

    releases_dir.mkdir(parents=True, exist_ok=True)
    file_basename = (
        f"v{version}"
        if not notes_input.tag.lower().startswith("v")
        else notes_input.tag
    )
    release_file = releases_dir / f"{file_basename}.html"

    if release_file.exists() and confirm_overwrite:
        if not prompt_yn(
            f"Release notes file already exists at '{release_file}'. Overwrite?",
        ):
            raise ReleaseError(
                f"refusing to overwrite existing release notes file: {release_file}"
            )

    release_file.write_text(entry_xml, encoding="utf-8")
    console.print(f"Wrote docs release notes: [bold]{release_file}[/bold]")
    return release_file


def fetch_release_body_html(
    client: httpx.Client,
    release_id: int,
    slug: RepoSlug,
) -> str:
    """Fetch the release body as HTML from the GitHub API.

    Makes two API calls:
    1. GET release details (JSON format) to get the markdown body as fallback
    2. GET release details (HTML format via Accept header)

    If the HTML body is empty but markdown exists, wraps markdown in <pre><code>.
    """
    release_url = f"https://api.github.com/repos/{slug.full}/releases/{release_id}"

    # Get markdown body as fallback
    details_json = github_api(client, "GET", release_url)
    markdown_body = ""
    if isinstance(details_json, dict):
        markdown_body = details_json.get("body", "") or ""

    # Get HTML body
    details_html = github_api(
        client,
        "GET",
        release_url,
        accept="application/vnd.github.v3.html+json",
    )
    body_html = ""
    if isinstance(details_html, dict):
        body_html = details_html.get("body_html", "") or ""

    if not body_html and markdown_body:
        body_html = f"<pre><code>{html.escape(markdown_body)}</code></pre>"

    return body_html
