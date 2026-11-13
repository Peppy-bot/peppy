"""The checks of the transcript against the docs.

Each check gives the list of the problems that it finds, as sentences for the
person who corrects the transcript. The checks compare text, counts and hashes
only, so the same files always give the same result.
"""

import builtins
import io
import keyword
from pathlib import Path
import re
import tokenize

from tools import markup
from tools.docs_sections import Section, SectionNotFound, read_section
from tools.transcript import CodeBlock, Segment, Transcript

# The limit that ASD-STE100 gives for a descriptive sentence.
MAX_WORDS_IN_A_SENTENCE = 25

PLACEHOLDER = re.compile(r"<[^<>]+>")
JSON5_STRING_OR_COMMENT = re.compile(r"\"(?:\\.|[^\"\\])*\"|'(?:\\.|[^'\\])*'|//[^\n]*|/\*.*?\*/", re.DOTALL)
NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
JSON5_LITERALS = frozenset({"true", "false", "null"})
PYTHON_WORDS = frozenset(keyword.kwlist) | frozenset(keyword.softkwlist) | frozenset(dir(builtins))


def source_problems(transcript: Transcript, docs_root: Path) -> list[str]:
    """The sources that name no section, or a section whose text changed."""
    problems: list[str] = []
    for segment in transcript.segments:
        for source in segment.sources:
            try:
                section = read_section(docs_root, source.page, source.section)
            except SectionNotFound as not_found:
                problems.append(f"Segment `{segment.id}`: {not_found}.")
                continue
            if section.sha256 != source.sha256:
                problems.append(
                    f"Segment `{segment.id}`: the text of the section `{source.section}` of {source.page} "
                    f"changed. Read the segment against the section, correct the transcript if necessary, "
                    f"then set the hash of this source to {section.sha256}."
                )
    return problems


def technical_name_problems(transcript: Transcript, docs_root: Path) -> list[str]:
    """The technical names of each segment that its sections do not hold."""
    problems: list[str] = []
    for segment in transcript.segments:
        sections_text = "\n".join(section.text for section in _found_sections(segment, docs_root))
        for where, fragment in required_fragments_of_segment(segment):
            if not holds_fragment(sections_text, fragment):
                problems.append(
                    f"Segment `{segment.id}`, {where}: no section that the segment names holds `{fragment}`."
                )
    return problems


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


def _found_sections(segment: Segment, docs_root: Path) -> list[Section]:
    sections: list[Section] = []
    for source in segment.sources:
        try:
            sections.append(read_section(docs_root, source.page, source.section))
        except SectionNotFound:
            continue
    return sections


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
