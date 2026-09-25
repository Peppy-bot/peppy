"""Tests for functions.release_notes."""

from __future__ import annotations

from pathlib import Path

import pytest

from functions.release_notes import (
    ReleaseNotesInput,
    generate_release_notes_file,
    release_notes_file,
)


@pytest.mark.parametrize(
    ("tag", "expected"),
    [
        ("v0.3.0", "docs/src/content/releases/v0.3.0.html"),
        # A tag without its 'v' still names the file the docs link to.
        ("0.3.0", "docs/src/content/releases/v0.3.0.html"),
    ],
)
def test_release_notes_file_is_named_after_the_tag(tag: str, expected: str) -> None:
    assert release_notes_file(tag) == Path(expected)


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
