"""The reader of the sections of the docs pages."""

from pathlib import Path

import pytest

from tools.docs_sections import SectionNotFound, headings_of, read_section

PAGE = """---
title: Standalone nodes
description: A page of the test
---
import { Code } from '@astrojs/starlight/components';
import standalonePython from './snippets/standalone.py?raw';

The text before the first heading.

## Debugging a node

Run the node as a regular program.

```sh
# A comment of a shell block is not a heading
peppy node sync
```

<Code code={standalonePython} lang="python" />

### A smaller section

The text of the smaller section.

## Seeding slots

The last section.
"""


@pytest.fixture
def docs_root(tmp_path: Path) -> Path:
    (tmp_path / "guides" / "snippets").mkdir(parents=True)
    (tmp_path / "guides" / "page.mdx").write_text(PAGE, encoding="utf-8")
    (tmp_path / "guides" / "snippets" / "standalone.py").write_text("NodeBuilder().run(setup)\n", encoding="utf-8")
    return tmp_path


def test_a_section_is_the_text_below_its_heading_up_to_the_next_heading(docs_root: Path) -> None:
    section = read_section(docs_root, "guides/page.mdx", "Debugging a node")

    assert section.text.startswith("Run the node as a regular program.")
    assert "The text of the smaller section." not in section.text
    assert "The last section." not in section.text


def test_a_section_holds_the_text_of_the_snippet_files_that_it_shows(docs_root: Path) -> None:
    section = read_section(docs_root, "guides/page.mdx", "Debugging a node")

    assert "NodeBuilder().run(setup)" in section.text


def test_the_title_of_the_page_names_the_text_before_the_first_heading(docs_root: Path) -> None:
    section = read_section(docs_root, "guides/page.mdx", "Standalone nodes")

    assert section.text == "The text before the first heading."


def test_a_comment_line_of_a_code_block_is_not_a_heading(docs_root: Path) -> None:
    assert headings_of(docs_root, "guides/page.mdx") == [
        "Standalone nodes",
        "Debugging a node",
        "A smaller section",
        "Seeding slots",
    ]


def test_the_hash_changes_when_the_text_of_the_section_changes(docs_root: Path) -> None:
    before = read_section(docs_root, "guides/page.mdx", "Seeding slots").sha256
    page = docs_root / "guides" / "page.mdx"
    page.write_text(page.read_text(encoding="utf-8").replace("The last section.", "The last sections."), encoding="utf-8")

    assert read_section(docs_root, "guides/page.mdx", "Seeding slots").sha256 != before


def test_the_hash_changes_when_a_snippet_file_of_the_section_changes(docs_root: Path) -> None:
    before = read_section(docs_root, "guides/page.mdx", "Debugging a node").sha256
    (docs_root / "guides" / "snippets" / "standalone.py").write_text("NodeBuilder().run(other)\n", encoding="utf-8")

    assert read_section(docs_root, "guides/page.mdx", "Debugging a node").sha256 != before


def test_a_space_at_the_end_of_a_line_does_not_change_the_hash(docs_root: Path) -> None:
    before = read_section(docs_root, "guides/page.mdx", "Seeding slots").sha256
    page = docs_root / "guides" / "page.mdx"
    page.write_text(page.read_text(encoding="utf-8").replace("The last section.", "The last section.  "), encoding="utf-8")

    assert read_section(docs_root, "guides/page.mdx", "Seeding slots").sha256 == before


@pytest.mark.parametrize(
    ("page", "heading", "reason"),
    [
        ("guides/absent.mdx", "Debugging a node", "the page does not exist"),
        ("guides/page.mdx", "No such heading", "no section has the heading"),
    ],
)
def test_a_source_that_names_no_section_is_refused(docs_root: Path, page: str, heading: str, reason: str) -> None:
    with pytest.raises(SectionNotFound, match=reason):
        read_section(docs_root, page, heading)


def test_a_heading_that_two_sections_have_is_refused(docs_root: Path) -> None:
    page = docs_root / "guides" / "page.mdx"
    page.write_text(page.read_text(encoding="utf-8") + "\n## Seeding slots\n\nAgain.\n", encoding="utf-8")

    with pytest.raises(SectionNotFound, match="more than one section"):
        read_section(docs_root, "guides/page.mdx", "Seeding slots")
