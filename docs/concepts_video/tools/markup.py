"""The marks of a text of the transcript.

A text of the transcript is prose. A span between two backticks is a technical
name: a command, a flag, a file name or a name of an API. In a span, a word in
angle brackets is a placeholder that the viewer replaces, as in
`peppy stack launch <launcher>`.
"""

import re
from typing import Mapping

CODE_SPAN = re.compile(r"`([^`]+)`")


def code_spans(text: str) -> list[str]:
    """The technical names of a text, in order."""
    return CODE_SPAN.findall(text)


def without_marks(text: str) -> str:
    """The text as a viewer reads it."""
    return text.replace("`", "")


def parts(text: str) -> list[tuple[bool, str]]:
    """The text in order, as parts that are a technical name or prose."""
    found: list[tuple[bool, str]] = []
    position = 0
    for span in CODE_SPAN.finditer(text):
        if span.start() > position:
            found.append((False, text[position : span.start()]))
        found.append((True, span.group(1)))
        position = span.end()
    if position < len(text):
        found.append((False, text[position:]))
    return found


def spoken(text: str, pronunciation: Mapping[str, str]) -> str:
    """The text as the voice says it.

    Each key of `pronunciation` that the text holds as full words becomes its
    value. The longest key goes first, so `peppy.json5` wins over `peppy`.
    """
    said = without_marks(text)
    for written in sorted(pronunciation, key=len, reverse=True):
        as_full_words = rf"(?<!\w){re.escape(written)}(?!\w)"
        said = re.sub(as_full_words, lambda _: pronunciation[written], said)
    return said


def sentences(text: str) -> list[str]:
    """The sentences of a text. A sentence ends with `.`, `?` or `!` and a space."""
    return [sentence for sentence in re.split(r"(?<=[.?!])\s+", without_marks(text).strip()) if sentence]


def word_count(sentence: str) -> int:
    return len(sentence.split())
