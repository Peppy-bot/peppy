"""The checks of the transcript: against the docs, and of its sentences.

The docs check gives, for each segment, what does not agree with the sections
of the docs that the segment names. The checks compare text, counts and hashes
only, so the same files always give the same result.
"""

import builtins
from dataclasses import dataclass
import difflib
import io
import keyword
from pathlib import Path
import re
import tokenize

from tools import markup
from tools.docs_sections import Section, SectionNotFound, read_section
from tools.transcript import CodeBlock, Segment, Source, Transcript

# The limit that ASD-STE100 gives for a descriptive sentence.
MAX_WORDS_IN_A_SENTENCE = 25

PLACEHOLDER = re.compile(r"<[^<>]+>")
JSON5_STRING_OR_COMMENT = re.compile(r"\"(?:\\.|[^\"\\])*\"|'(?:\\.|[^'\\])*'|//[^\n]*|/\*.*?\*/", re.DOTALL)
NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
JSON5_LITERALS = frozenset({"true", "false", "null"})
PYTHON_WORDS = frozenset(keyword.kwlist) | frozenset(keyword.softkwlist) | frozenset(dir(builtins))


@dataclass(frozen=True)
class ChangedSection:
    """A section whose text is not the text that the transcript records."""

    page: str
    section: str
    # The lines that changed since the baseline, as a unified diff. None when
    # the check has no baseline, or when the baseline has no such section.
    diff: str | None


@dataclass(frozen=True)
class MissingName:
    """A technical name of a segment that no section of the segment holds."""

    # The sentence or the text of the screen that shows the name.
    where: str
    name: str


@dataclass(frozen=True)
class SegmentFindings:
    """What the docs check found for one segment."""

    segment_id: str
    changed_sections: tuple[ChangedSection, ...]
    # For each source that names no section, the reason.
    missing_sections: tuple[str, ...]
    missing_names: tuple[MissingName, ...]

    @property
    def is_empty(self) -> bool:
        return not (self.changed_sections or self.missing_sections or self.missing_names)


def docs_findings(transcript: Transcript, docs_root: Path, baseline_root: Path | None = None) -> list[SegmentFindings]:
    """What does not agree with the docs, for each segment that has a finding.

    `baseline_root` holds the docs pages of the last release. A section that
    has the text of the baseline is not a changed section: the release that
    shipped this text read the segment against it.
    """
    findings = [_findings_of_segment(segment, docs_root, baseline_root) for segment in transcript.segments]
    return [found for found in findings if not found.is_empty]


def problems_of(findings: list[SegmentFindings]) -> list[str]:
    """The findings as sentences for the person who corrects the transcript."""
    problems: list[str] = []
    for found in findings:
        problems += [f"Segment `{found.segment_id}`: {reason}." for reason in found.missing_sections]
        problems += [
            f"Segment `{found.segment_id}`: the text of the section `{changed.section}` of {changed.page} changed. "
            "Read the segment against the section, correct the transcript if necessary, "
            "then run `pixi run record-sections`."
            for changed in found.changed_sections
        ]
        problems += [
            f"Segment `{found.segment_id}`, {missing.where}: no section that the segment names holds `{missing.name}`."
            for missing in found.missing_names
        ]
    return problems


def _findings_of_segment(segment: Segment, docs_root: Path, baseline_root: Path | None) -> SegmentFindings:
    changed: list[ChangedSection] = []
    missing_sections: list[str] = []
    sections: list[Section] = []
    for source in segment.sources:
        try:
            section = read_section(docs_root, source.page, source.section)
        except SectionNotFound as not_found:
            missing_sections.append(str(not_found))
            continue
        sections.append(section)
        if section.sha256 == source.sha256:
            continue
        baseline = _baseline_section(baseline_root, source)
        if baseline is not None and baseline.text == section.text:
            continue
        changed.append(
            ChangedSection(
                page=source.page,
                section=source.section,
                diff=None if baseline is None else _diff(baseline.text, section.text),
            )
        )
    sections_text = "\n".join(section.text for section in sections)
    return SegmentFindings(
        segment_id=segment.id,
        changed_sections=tuple(changed),
        missing_sections=tuple(missing_sections),
        missing_names=tuple(
            MissingName(where=where, name=fragment)
            for where, fragment in required_fragments_of_segment(segment)
            if not holds_fragment(sections_text, fragment)
        ),
    )


def _baseline_section(baseline_root: Path | None, source: Source) -> Section | None:
    if baseline_root is None:
        return None
    try:
        return read_section(baseline_root, source.page, source.section)
    except SectionNotFound:
        return None


def _diff(before: str, after: str) -> str:
    lines = difflib.unified_diff(before.splitlines(), after.splitlines(), "last release", "now", lineterm="")
    return "\n".join(lines)


def sentence_length_problems(transcript: Transcript) -> list[str]:
    """The sentences of the voice that are longer than the limit of ASD-STE100."""
    problems: list[str] = []
    for segment in transcript.segments:
        for entry in segment.voice:
            for sentence in markup.sentences(entry.text):
                words = markup.word_count(sentence)
                if words > MAX_WORDS_IN_A_SENTENCE:
                    problems.append(
                        f"Segment `{segment.id}`, sentence `{entry.id}`: {words} words, "
                        f"and the limit is {MAX_WORDS_IN_A_SENTENCE}: {sentence}"
                    )
    return problems


def required_fragments_of_segment(segment: Segment) -> list[tuple[str, str]]:
    """Each fragment that the sections of the segment must hold, with its place."""
    required: list[tuple[str, str]] = []
    for entry in segment.voice:
        for span in markup.code_spans(entry.text):
            required += [(f"sentence `{entry.id}`", fragment) for fragment in fragments_of_span(span)]
    for path, leaf in segment.screen_leaves():
        if isinstance(leaf, CodeBlock):
            required += [(path, fragment) for fragment in fragments_of_code_block(leaf)]
            continue
        for span in markup.code_spans(leaf):
            required += [(path, fragment) for fragment in fragments_of_span(span)]
    return required


def fragments_of_span(span: str) -> list[str]:
    """The parts of a technical name that the docs must hold.

    A placeholder and a word with no letter and no digit, such as `|`, are not
    parts. Each flag is a part. Each run of the other words is a part, so the
    docs must hold `peppy stack launch` as these three words in this order.
    """
    fragments: list[str] = []
    for literal in PLACEHOLDER.split(span):
        run: list[str] = []
        for word in literal.split():
            if not _has_letter_or_digit(word):
                _end_run(run, fragments)
                continue
            if word.startswith("-"):
                _end_run(run, fragments)
                fragments.append(word)
                continue
            run.append(word)
        _end_run(run, fragments)
    return fragments


def fragments_of_code_block(block: CodeBlock) -> list[str]:
    """The names of a code block that the docs must hold.

    A line of a shell block is a command. In a Python block and in a JSON5
    block, each name must be in the docs, but for the words of the language
    and for the names that the block gives as names of its example.
    """
    if block.language == "sh":
        return [fragment for line in block.lines for fragment in fragments_of_span(line)]
    names = _python_names(block.code) if block.language == "python" else _json5_names(block.code)
    return [name for name in names if name not in block.example_names]


def holds_fragment(text: str, fragment: str) -> bool:
    """True when `text` holds the words of `fragment` in order, as full words."""
    words = r"\s+".join(re.escape(word) for word in fragment.split())
    return re.search(rf"(?<![\w-]){words}(?![\w-])", text) is not None


def _end_run(run: list[str], fragments: list[str]) -> None:
    if run:
        fragments.append(" ".join(run))
        run.clear()


def _has_letter_or_digit(word: str) -> bool:
    return any(character.isalnum() for character in word)


def _python_names(code: str) -> list[str]:
    tokens = tokenize.generate_tokens(io.StringIO(code + "\n").readline)
    names = [token.string for token in tokens if token.type == tokenize.NAME]
    return _unique(name for name in names if name not in PYTHON_WORDS)


def _json5_names(code: str) -> list[str]:
    outside_strings = JSON5_STRING_OR_COMMENT.sub(" ", code)
    return _unique(name for name in NAME.findall(outside_strings) if name not in JSON5_LITERALS)


def _unique(names) -> list[str]:
    return list(dict.fromkeys(names))
