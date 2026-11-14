"""The command that records the present text of the docs sections."""

from pathlib import Path
import tomllib

import pytest

from tools import record_sections
from tools.docs_sections import read_section
from tools.transcript import parse_transcript

PAGE = """---
title: A page
---

The text below the title.

## A section

The text of the section.
"""

TRANSCRIPT = """# A comment of the file.
[voice]
id = "af_heart"
speed = 1.0
language = "en-us"
lead_in_seconds = 1.0
sentence_gap_seconds = 0.5
tail_seconds = 2.0

[labels]
term = "Term"

[[segment]]
id = "opening"
chapter = "A chapter"

[[segment.source]]
page = "guides/page.mdx"
section = "A section"
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"  # The first source.

[[segment.source]]
page = "guides/page.mdx"
section = "No such section"
sha256 = "1111111111111111111111111111111111111111111111111111111111111111"

[[segment.voice]]
id = "only"
text = "One sentence."

[segment.screen]
title = "A title"
sha256 = "2222222222222222222222222222222222222222222222222222222222222222"

[[segment]]
id = "summary"
chapter = "The last chapter"

[[segment.source]]
page = "guides/page.mdx"
section = "A page"
sha256 = "3333333333333333333333333333333333333333333333333333333333333333"

[[segment.voice]]
id = "only"
text = "The last sentence."

[segment.screen]
title = "The last title"
"""


@pytest.fixture
def docs_root(tmp_path: Path) -> Path:
    (tmp_path / "guides").mkdir()
    (tmp_path / "guides" / "page.mdx").write_text(PAGE, encoding="utf-8")
    return tmp_path


def test_each_source_gets_the_hash_of_the_present_text_of_its_section(docs_root: Path) -> None:
    transcript = parse_transcript(tomllib.loads(TRANSCRIPT))

    recorded = record_sections.with_hashes(TRANSCRIPT, record_sections.present_hashes(transcript, docs_root))

    sources = [segment.sources for segment in parse_transcript(tomllib.loads(recorded)).segments]
    assert sources[0][0].sha256 == read_section(docs_root, "guides/page.mdx", "A section").sha256
    assert sources[1][0].sha256 == read_section(docs_root, "guides/page.mdx", "A page").sha256


def test_a_source_that_names_no_section_keeps_its_hash(docs_root: Path) -> None:
    transcript = parse_transcript(tomllib.loads(TRANSCRIPT))

    assert record_sections.present_hashes(transcript, docs_root)[0][1] == "1" * 64


def test_the_command_changes_the_hash_lines_of_the_sources_only(docs_root: Path) -> None:
    transcript = parse_transcript(tomllib.loads(TRANSCRIPT))

    recorded = record_sections.with_hashes(TRANSCRIPT, record_sections.present_hashes(transcript, docs_root))

    changed = [
        (old, new) for old, new in zip(TRANSCRIPT.split("\n"), recorded.split("\n"), strict=True) if old != new
    ]
    assert len(changed) == 2
    assert all(old.startswith('sha256 = "') and new.startswith('sha256 = "') for old, new in changed)
    assert changed[0][1].endswith("  # The first source.")
    assert 'sha256 = "' + "2" * 64 + '"' in recorded


def test_a_transcript_that_has_the_present_hashes_stays_the_same(docs_root: Path) -> None:
    transcript = parse_transcript(tomllib.loads(TRANSCRIPT))
    recorded = record_sections.with_hashes(TRANSCRIPT, record_sections.present_hashes(transcript, docs_root))

    again = record_sections.with_hashes(recorded, record_sections.present_hashes(transcript, docs_root))

    assert again == recorded


def test_sources_in_a_form_that_the_command_does_not_write_are_refused() -> None:
    inline = TRANSCRIPT.replace(
        '[[segment.source]]\npage = "guides/page.mdx"\nsection = "A page"\nsha256 = "' + "3" * 64 + '"\n',
        'source = [{ page = "guides/page.mdx", section = "A page", sha256 = "' + "3" * 64 + '" }]\n',
    )
    assert inline != TRANSCRIPT

    with pytest.raises(record_sections.CannotRecord, match="cannot write each hash"):
        record_sections.with_hashes(inline, [["a" * 64, "b" * 64], ["c" * 64]])
