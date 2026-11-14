"""The transcript: the only source of the voice and of the texts on the screen.

`transcript.toml` has one `[[segment]]` for each segment of the video. A
segment names the sections of the docs that it rests on, with the hash of the
text of each one. It holds the sentences of the voice in order, and the texts
that its composition shows. `[splash]` holds the splash screen, which shows
before the first segment with no voice. `[[act]]` holds each act: a group of
segments that follow each other, which the screen shows as a part of the video.
"""

from dataclasses import dataclass, replace
from itertools import groupby
from pathlib import Path
import re
import tomllib
from typing import Iterator, Mapping, Union

from tools import markup

IDENTIFIER = re.compile(r"^[a-z][a-z0-9_]*$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
# The languages of the code blocks that the checks can read.
CODE_LANGUAGES = frozenset({"python", "json5", "sh"})
# The id of the composition of the splash screen. No segment has this id.
SPLASH_ID = "splash"


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
class Splash:
    # How long the splash screen shows.
    seconds: float
    screen: Mapping[str, str]


@dataclass(frozen=True)
class Chapter:
    """One segment as the map of the video shows it."""

    segment_id: str
    # The place of the segment in the video, from 1.
    number: int
    title: str


@dataclass(frozen=True)
class Act:
    id: str
    # The place of the act in the video, from 1.
    number: int
    title: str
    # The question that the segments of the act answer.
    question: str
    # The segments of the act, in order. The video of one part keeps them all,
    # so its map is the map of the full video.
    chapters: tuple[Chapter, ...]

    def chapter_index(self, segment_id: str) -> int:
        """The place of a segment in the act, from 0."""
        return [chapter.segment_id for chapter in self.chapters].index(segment_id)


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
    # The place of the segment in the video, from 1.
    number: int
    chapter: str
    # The id of the act of the segment. A segment can be in no act.
    act: str | None
    sources: tuple[Source, ...]
    voice: tuple[Sentence, ...]
    screen: Mapping[str, ScreenText]

    def screen_leaves(self) -> Iterator[tuple[str, str | CodeBlock]]:
        """Each text and each code block of the screen, with its dotted path."""
        return screen_leaves(self.screen)


@dataclass(frozen=True)
class Transcript:
    voice: Voice
    # How the voice says a written word, for the words that it reads wrongly.
    pronunciation: Mapping[str, str]
    # The words of the frames of the cards, which each composition shows.
    labels: Mapping[str, str]
    # The splash screen of the start of the video, when the video has one.
    splash: Splash | None
    acts: tuple[Act, ...]
    segments: tuple[Segment, ...]

    def act(self, act_id: str) -> Act:
        for act in self.acts:
            if act.id == act_id:
                return act
        raise KeyError(act_id)

    def segment(self, segment_id: str) -> Segment:
        for segment in self.segments:
            if segment.id == segment_id:
                return segment
        raise KeyError(segment_id)

    @property
    def part_ids(self) -> list[str]:
        """The id of each part that has a video of its own: the splash screen and each segment."""
        splash_ids = [] if self.splash is None else [SPLASH_ID]
        return splash_ids + [segment.id for segment in self.segments]

    def part(self, part_id: str) -> "Transcript":
        """The transcript of the video of one part.

        The video of a segment has no splash screen. The video of the splash
        screen has the first segment after it, because a video has a voice.
        """
        if part_id not in self.part_ids:
            raise KeyError(part_id)
        if part_id == SPLASH_ID:
            return replace(self, segments=self.segments[:1])
        return replace(self, splash=None, segments=(self.segment(part_id),))


def read_transcript(path: Path) -> Transcript:
    with path.open("rb") as file:
        document = tomllib.load(file)
    return parse_transcript(document)


def parse_transcript(document: Mapping[str, object]) -> Transcript:
    segments = tuple(
        _parse_segment(table, number)
        for number, table in enumerate(_list_of_tables(document, "segment", "transcript"), start=1)
    )
    if not segments:
        raise TranscriptError("transcript: it has no segment")
    _refuse_duplicates([segment.id for segment in segments], "transcript")
    if any(segment.id == SPLASH_ID for segment in segments):
        raise TranscriptError(f"transcript: a segment has the id `{SPLASH_ID}`, which is the id of the splash screen")
    acts = tuple(
        _parse_act(table, number, segments)
        for number, table in enumerate(_list_of_tables(document, "act", "transcript"), start=1)
    )
    _refuse_duplicates([act.id for act in acts], "transcript, act")
    _refuse_acts_that_the_segments_do_not_fill(acts, segments)
    return Transcript(
        voice=_parse_voice(_table(document, "voice", "transcript")),
        pronunciation=_parse_texts(document.get("pronunciation", {}), "pronunciation"),
        labels=_parse_texts(_table(document, "labels", "transcript"), "labels"),
        splash=_parse_splash(_table(document, "splash", "transcript")) if "splash" in document else None,
        acts=acts,
        segments=segments,
    )


def _parse_act(table: Mapping[str, object], number: int, segments: tuple[Segment, ...]) -> Act:
    act_id = _identifier(table, "id", f"act {number}")
    where = f"act `{act_id}`"
    return Act(
        id=act_id,
        number=number,
        title=_string(table, "title", where),
        question=_string(table, "question", where),
        chapters=tuple(
            Chapter(segment_id=segment.id, number=segment.number, title=segment.chapter)
            for segment in segments
            if segment.act == act_id
        ),
    )


def _refuse_acts_that_the_segments_do_not_fill(acts: tuple[Act, ...], segments: tuple[Segment, ...]) -> None:
    """Refuse a segment that names no act of the transcript, and acts whose segments are not in order.

    Each act has segments, the segments of an act follow each other, and the
    acts come in the order of their segments.
    """
    known = [act.id for act in acts]
    for segment in segments:
        if segment.act is not None and segment.act not in known:
            raise TranscriptError(
                f"segment `{segment.id}`: `act` is `{segment.act}`, and the transcript has no such act"
            )
    runs = [act_id for act_id, _ in groupby(segment.act for segment in segments) if act_id is not None]
    if runs != known:
        raise TranscriptError(
            f"transcript: the segments give the acts {runs}, and the acts are {known}: "
            "each act has segments, which follow each other, in the order of the acts"
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


def _parse_splash(table: Mapping[str, object]) -> Splash:
    """The splash screen. It names no section of the docs, so its texts hold no technical name."""
    screen = _parse_texts(_table(table, "screen", "splash"), "splash.screen")
    for key, text in screen.items():
        if markup.without_marks(text) != text:
            raise TranscriptError(
                f"splash.screen.{key}: it holds a technical name, and the splash screen names no section of the docs"
            )
    return Splash(seconds=_positive_number(table, "seconds", "splash"), screen=screen)


def _parse_texts(table: object, where: str) -> Mapping[str, str]:
    if not isinstance(table, dict):
        raise TranscriptError(f"{where}: it is not a table")
    for key, text in table.items():
        if not isinstance(text, str) or not text.strip():
            raise TranscriptError(f"{where}.{key}: it is not a text")
    return dict(table)


def _parse_segment(table: Mapping[str, object], number: int) -> Segment:
    segment_id = _identifier(table, "id", f"segment {number}")
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
        number=number,
        chapter=_string(table, "chapter", where),
        act=_identifier(table, "act", where) if "act" in table else None,
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


def screen_leaves(screen: Mapping[str, ScreenText]) -> Iterator[tuple[str, str | CodeBlock]]:
    """Each text and each code block of a screen table, with its dotted path."""
    yield from _leaves("screen", screen)


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
