"""The sections of the docs pages that the transcript rests on.

A section is the text that a reader sees below one heading of a page, up to
the next heading of any level. The title of a page is the heading of the text
that comes before its first heading. Where a section shows a snippet file
through `<Code code={name} />`, the text of the file is part of the section.
"""

from dataclasses import dataclass
import hashlib
from pathlib import Path
import re

HEADING = re.compile(r"^(#{1,6})\s+(.+?)\s*$")
FENCE = re.compile(r"^\s*(`{3,}|~{3,})(.*)$")
FRONTMATTER_FENCE = "---"
TITLE = re.compile(r"^title:\s*(.+?)\s*$")
IMPORT = re.compile(r"^import\s.+\sfrom\s+['\"](.+?)['\"];?\s*$")
RAW_IMPORT = re.compile(r"^import\s+(\w+)\s+from\s+['\"](.+?)\?raw['\"];?\s*$")
SNIPPET_REFERENCE = re.compile(r"code=\{(\w+)\}")


class SectionNotFound(Exception):
    """The page or the heading that a source names does not exist."""


@dataclass(frozen=True)
class Section:
    page: str
    heading: str
    text: str

    @property
    def sha256(self) -> str:
        return hashlib.sha256(self.text.encode("utf-8")).hexdigest()


@dataclass(frozen=True)
class _Page:
    title: str
    # The names of the snippet files that the page imports as text.
    snippets: dict[str, Path]
    # The lines of the page below its frontmatter and its imports.
    body: list[str]


def read_section(docs_root: Path, page: str, heading: str) -> Section:
    """The section of `page` below `heading`, with its snippet files."""
    page_path = docs_root / page
    if not page_path.is_file():
        raise SectionNotFound(f"{page}: the page does not exist")
    parsed = _parse_page(page_path)
    own_lines = _own_lines(parsed, page, heading)
    snippet_texts = [
        parsed.snippets[name].read_text(encoding="utf-8")
        for name in _referenced_snippets(own_lines)
        if name in parsed.snippets
    ]
    return Section(page=page, heading=heading, text=_normalize(own_lines, snippet_texts))


def headings_of(docs_root: Path, page: str) -> list[str]:
    """The headings that name a section of `page`, the title first."""
    parsed = _parse_page(docs_root / page)
    return [parsed.title] + [text for _, text in _headings(parsed.body)]


def _parse_page(page_path: Path) -> _Page:
    lines = page_path.read_text(encoding="utf-8").splitlines()
    frontmatter, rest = _split_frontmatter(lines)
    title = _title(frontmatter, page_path)
    snippets: dict[str, Path] = {}
    body: list[str] = []
    code_blocks = _FenceTracker()
    for line in rest:
        if not code_blocks.is_code_line(line):
            raw_import = RAW_IMPORT.match(line)
            if raw_import:
                snippets[raw_import.group(1)] = (page_path.parent / raw_import.group(2)).resolve()
                continue
            if IMPORT.match(line):
                continue
        body.append(line)
    return _Page(title=title, snippets=snippets, body=body)


def _split_frontmatter(lines: list[str]) -> tuple[list[str], list[str]]:
    if not lines or lines[0].strip() != FRONTMATTER_FENCE:
        return [], lines
    for index in range(1, len(lines)):
        if lines[index].strip() == FRONTMATTER_FENCE:
            return lines[1:index], lines[index + 1 :]
    return [], lines


def _title(frontmatter: list[str], page_path: Path) -> str:
    for line in frontmatter:
        match = TITLE.match(line)
        if match:
            return match.group(1).strip("'\"")
    raise SectionNotFound(f"{page_path.name}: the page has no title")


class _FenceTracker:
    """Tells whether a line is inside a fenced code block."""

    def __init__(self) -> None:
        self._open_marker: str | None = None

    def is_code_line(self, line: str) -> bool:
        """True when `line` is in a code block or is one of its two fences.

        Call it one time for each line of the page, in order.
        """
        fence = FENCE.match(line)
        if self._open_marker is None:
            if fence:
                self._open_marker = fence.group(1)
                return True
            return False
        if fence and self._closes(fence):
            self._open_marker = None
        return True

    def _closes(self, fence: re.Match[str]) -> bool:
        marker, rest = fence.group(1), fence.group(2)
        same_kind = marker[0] == self._open_marker[0]
        return same_kind and len(marker) >= len(self._open_marker) and not rest.strip()


def _headings(body: list[str]) -> list[tuple[int, str]]:
    """The line index and the text of each heading outside of the code blocks."""
    found: list[tuple[int, str]] = []
    code_blocks = _FenceTracker()
    for index, line in enumerate(body):
        if code_blocks.is_code_line(line):
            continue
        match = HEADING.match(line)
        if match:
            found.append((index, match.group(2)))
    return found


def _own_lines(parsed: _Page, page: str, heading: str) -> list[str]:
    headings = _headings(parsed.body)
    positions = [position for position, (_, text) in enumerate(headings) if text == heading]
    is_title = heading == parsed.title
    if len(positions) + int(is_title) > 1:
        raise SectionNotFound(f"{page}: more than one section has the heading `{heading}`")
    if is_title:
        first_heading_line = headings[0][0] if headings else len(parsed.body)
        return parsed.body[:first_heading_line]
    if not positions:
        raise SectionNotFound(f"{page}: no section has the heading `{heading}`")
    position = positions[0]
    start = headings[position][0] + 1
    end = headings[position + 1][0] if position + 1 < len(headings) else len(parsed.body)
    return parsed.body[start:end]


def _referenced_snippets(lines: list[str]) -> list[str]:
    names: list[str] = []
    for line in lines:
        for name in SNIPPET_REFERENCE.findall(line):
            if name not in names:
                names.append(name)
    return names


def _normalize(own_lines: list[str], snippet_texts: list[str]) -> str:
    """One text with no space at the end of a line and no empty line at its ends."""
    parts = ["\n".join(own_lines)] + snippet_texts
    lines = [line.rstrip() for part in parts for line in part.splitlines()]
    return "\n".join(lines).strip("\n")
