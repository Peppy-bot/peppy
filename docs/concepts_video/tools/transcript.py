"""The transcript: the only source of the voice and of the texts on the screen.

`transcript.toml` has one `[[segment]]` for each segment of the video. A
segment names the sections of the docs that it rests on, with the hash of the
text of each one. It holds the sentences of the voice in order, and the texts
that its composition shows.
"""

from dataclasses import dataclass
from pathlib import Path
import re
import tomllib
from typing import Iterator, Mapping, Union

IDENTIFIER = re.compile(r"^[a-z][a-z0-9_]*$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
# The languages of the code blocks that the checks can read.
CODE_LANGUAGES = frozenset({"python", "json5", "sh"})


class TranscriptError(Exception):
    """The transcript file does not have the form that this module reads."""


@dataclass(frozen=True)
class Voice:
    id: str
    speed: float
    language: str
    # The silence before the first sentence of a segment, between two
    # sentences, and after the last sentence of a segment.
    lead_in_seconds: float
    sentence_gap_seconds: float
    tail_seconds: float


@dataclass(frozen=True)
class Source:
    page: str
    section: str
    sha256: str


@dataclass(frozen=True)
class Sentence:
    id: str
    text: str
    # The silence after the sentence, when it is not the usual one.
    pause_after_seconds: float | None


@dataclass(frozen=True)
class CodeBlock:
    language: str
    code: str
    # The names of the code that belong to the example and not to an API,
    # such as the generic camera of the examples.
    example_names: tuple[str, ...]

    @property
    def lines(self) -> list[str]:
        return self.code.splitlines()


ScreenText = Union[str, CodeBlock, tuple["ScreenText", ...], Mapping[str, "ScreenText"]]


@dataclass(frozen=True)
class Segment:
    id: str
    chapter: str
    sources: tuple[Source, ...]
    voice: tuple[Sentence, ...]
    screen: Mapping[str, ScreenText]

    def screen_leaves(self) -> Iterator[tuple[str, str | CodeBlock]]:
        """Each text and each code block of the screen, with its dotted path."""
        yield from _leaves("screen", self.screen)


@dataclass(frozen=True)
class Transcript:
    voice: Voice
    # How the voice says a written word, for the words that it reads wrongly.
    pronunciation: Mapping[str, str]
    # The words of the frames of the cards, which each composition shows.
    labels: Mapping[str, str]
    segments: tuple[Segment, ...]

    def segment(self, segment_id: str) -> Segment:
        for segment in self.segments:
            if segment.id == segment_id:
                return segment
        raise KeyError(segment_id)


def read_transcript(path: Path) -> Transcript:
    with path.open("rb") as file:
        document = tomllib.load(file)
    return parse_transcript(document)


def parse_transcript(document: Mapping[str, object]) -> Transcript:
    segments = tuple(
        _parse_segment(table, f"segment[{index}]")
        for index, table in enumerate(_list_of_tables(document, "segment", "transcript"))
    )
    if not segments:
        raise TranscriptError("transcript: it has no segment")
    _refuse_duplicates([segment.id for segment in segments], "transcript")
    return Transcript(
        voice=_parse_voice(_table(document, "voice", "transcript")),
        pronunciation=_parse_texts(document.get("pronunciation", {}), "pronunciation"),
        labels=_parse_texts(_table(document, "labels", "transcript"), "labels"),
        segments=segments,
    )


def _parse_voice(table: Mapping[str, object]) -> Voice:
    return Voice(
        id=_string(table, "id", "voice"),
        speed=_positive_number(table, "speed", "voice"),
        language=_string(table, "language", "voice"),
        lead_in_seconds=_positive_number(table, "lead_in_seconds", "voice"),
        sentence_gap_seconds=_positive_number(table, "sentence_gap_seconds", "voice"),
        tail_seconds=_positive_number(table, "tail_seconds", "voice"),
    )


def _parse_texts(table: object, where: str) -> Mapping[str, str]:
    if not isinstance(table, dict):
        raise TranscriptError(f"{where}: it is not a table")
    for key, text in table.items():
        if not isinstance(text, str) or not text.strip():
            raise TranscriptError(f"{where}.{key}: it is not a text")
    return dict(table)


def _parse_segment(table: Mapping[str, object], where: str) -> Segment:
    segment_id = _identifier(table, "id", where)
    where = f"segment `{segment_id}`"
    sources = tuple(
        _parse_source(source, f"{where}, source[{index}]")
        for index, source in enumerate(_list_of_tables(table, "source", where))
    )
    voice = tuple(
        _parse_sentence(sentence, f"{where}, voice[{index}]")
        for index, sentence in enumerate(_list_of_tables(table, "voice", where))
    )
    if not sources:
        raise TranscriptError(f"{where}: it names no section of the docs")
    if not voice:
        raise TranscriptError(f"{where}: it has no sentence of the voice")
    _refuse_duplicates([sentence.id for sentence in voice], f"{where}, voice")
    return Segment(
        id=segment_id,
        chapter=_string(table, "chapter", where),
        sources=sources,
        voice=voice,
        screen=_parse_screen_table(_table(table, "screen", where), f"{where}, screen"),
    )


def _parse_source(table: Mapping[str, object], where: str) -> Source:
    sha256 = _string(table, "sha256", where)
    if not SHA256.match(sha256):
        raise TranscriptError(f"{where}: `sha256` is not a SHA-256 in lower-case hexadecimal")
    return Source(page=_string(table, "page", where), section=_string(table, "section", where), sha256=sha256)


def _parse_sentence(table: Mapping[str, object], where: str) -> Sentence:
    pause = table.get("pause_after_seconds")
    if pause is not None and (not _is_number(pause) or pause < 0):
        raise TranscriptError(f"{where}: `pause_after_seconds` is not a number of seconds")
    return Sentence(
        id=_identifier(table, "id", where),
        text=_string(table, "text", where),
        pause_after_seconds=None if pause is None else float(pause),
    )


def _parse_screen_table(table: Mapping[str, object], where: str) -> Mapping[str, ScreenText]:
    return {key: _parse_screen_text(value, f"{where}.{key}") for key, value in table.items()}


def _parse_screen_text(value: object, where: str) -> ScreenText:
    if isinstance(value, str):
        if not value.strip():
            raise TranscriptError(f"{where}: the text is blank")
        return value
    if isinstance(value, list):
        return tuple(_parse_screen_text(item, f"{where}[{index}]") for index, item in enumerate(value))
    if isinstance(value, dict) and isinstance(value.get("code"), str):
        return _parse_code_block(value, where)
    if isinstance(value, dict):
        return _parse_screen_table(value, where)
    raise TranscriptError(f"{where}: it is not a text, a list, a table or a code block")


def _parse_code_block(table: Mapping[str, object], where: str) -> CodeBlock:
    example_names = table.get("example_names", [])
    if not isinstance(example_names, list) or not all(isinstance(name, str) for name in example_names):
        raise TranscriptError(f"{where}: `example_names` is not a list of names")
    unknown_keys = set(table) - {"code", "language", "example_names"}
    if unknown_keys:
        raise TranscriptError(f"{where}: a code block has no key {sorted(unknown_keys)}")
    language = _string(table, "language", where)
    if language not in CODE_LANGUAGES:
        raise TranscriptError(f"{where}: `language` is `{language}`, and it is one of {sorted(CODE_LANGUAGES)}")
    return CodeBlock(
        language=language,
        code=_string(table, "code", where).strip("\n"),
        example_names=tuple(example_names),
    )


def _leaves(path: str, value: ScreenText) -> Iterator[tuple[str, str | CodeBlock]]:
    if isinstance(value, (str, CodeBlock)):
        yield path, value
        return
    if isinstance(value, tuple):
        for index, item in enumerate(value):
            yield from _leaves(f"{path}[{index}]", item)
        return
    for key, item in value.items():
        yield from _leaves(f"{path}.{key}", item)


def _table(parent: Mapping[str, object], key: str, where: str) -> Mapping[str, object]:
    value = parent.get(key)
    if not isinstance(value, dict):
        raise TranscriptError(f"{where}: `{key}` is not a table")
    return value


def _list_of_tables(parent: Mapping[str, object], key: str, where: str) -> list[Mapping[str, object]]:
    value = parent.get(key, [])
    if not isinstance(value, list) or not all(isinstance(item, dict) for item in value):
        raise TranscriptError(f"{where}: `{key}` is not a list of tables")
    return value


def _string(table: Mapping[str, object], key: str, where: str) -> str:
    value = table.get(key)
    if not isinstance(value, str) or not value.strip():
        raise TranscriptError(f"{where}: `{key}` is not a text")
    return value


def _identifier(table: Mapping[str, object], key: str, where: str) -> str:
    value = _string(table, key, where)
    if not IDENTIFIER.match(value):
        raise TranscriptError(
            f"{where}: `{key}` is `{value}`, and an id has only lower-case letters, digits and `_`, with a letter first"
        )
    return value


def _positive_number(table: Mapping[str, object], key: str, where: str) -> float:
    value = table.get(key)
    if not _is_number(value) or value <= 0:
        raise TranscriptError(f"{where}: `{key}` is not a number above zero")
    return float(value)


def _is_number(value: object) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def _refuse_duplicates(ids: list[str], where: str) -> None:
    seen: set[str] = set()
    for item in ids:
        if item in seen:
            raise TranscriptError(f"{where}: two entries have the id `{item}`")
        seen.add(item)
