"""The record of the duration of the voice of each sentence.

`voice_durations.json` holds, for each sentence of the transcript, how long
the voice says it and a hash of what made that voice: the text as the voice
says it, the settings of the voice and the tools of the voice. The build
writes the record. With it, the timeline of the video needs no model, so the
checks and the lint of the compositions need none.
"""

from dataclasses import dataclass
import hashlib
import json
from pathlib import Path
from typing import Iterator, Mapping

from tools import markup
from tools.timing import sentence_key
from tools.transcript import Transcript, Voice


@dataclass(frozen=True)
class RecordedDuration:
    seconds: float
    spoken_sha256: str


def read_voice_durations(path: Path) -> dict[str, RecordedDuration]:
    """The record by `sentence_key`. A project with no record has an empty one."""
    if not path.is_file():
        return {}
    document = json.loads(path.read_text(encoding="utf-8"))
    return {
        key: RecordedDuration(seconds=entry["seconds"], spoken_sha256=entry["spoken_sha256"])
        for key, entry in document.items()
    }


def write_voice_durations(path: Path, recorded: Mapping[str, RecordedDuration]) -> None:
    document = {
        key: {"seconds": entry.seconds, "spoken_sha256": entry.spoken_sha256} for key, entry in recorded.items()
    }
    path.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")


def spoken_sentences(transcript: Transcript, voice_tools_identity: str) -> Iterator[tuple[str, str, str]]:
    """For each sentence in order: its key, its text as the voice says it, and its hash."""
    for segment in transcript.segments:
        for sentence in segment.voice:
            spoken = markup.spoken(sentence.text, transcript.pronunciation)
            yield (
                sentence_key(segment.id, sentence.id),
                spoken,
                spoken_sha256(spoken, transcript.voice, voice_tools_identity),
            )


def spoken_sha256(spoken_text: str, voice: Voice, voice_tools_identity: str) -> str:
    made_from = {
        "text": spoken_text,
        "voice": voice.id,
        "speed": voice.speed,
        "language": voice.language,
        "tools": voice_tools_identity,
    }
    return hashlib.sha256(json.dumps(made_from, sort_keys=True).encode("utf-8")).hexdigest()


def out_of_date_problems(
    transcript: Transcript, recorded: Mapping[str, RecordedDuration], voice_tools_identity: str
) -> list[str]:
    """Why the record does not agree with the transcript and the tools of the voice."""
    problems: list[str] = []
    expected_keys: list[str] = []
    for key, _, sha256 in spoken_sentences(transcript, voice_tools_identity):
        expected_keys.append(key)
        if key not in recorded:
            problems.append(f"voice_durations.json has no duration for the sentence `{key}`.")
        elif recorded[key].spoken_sha256 != sha256:
            problems.append(f"voice_durations.json has the duration of another voice for the sentence `{key}`.")
    for key in recorded:
        if key not in expected_keys:
            problems.append(f"voice_durations.json has a duration for `{key}`, which is not a sentence.")
    if list(recorded) != expected_keys and not problems:
        problems.append("voice_durations.json does not have the sentences in the order of the transcript.")
    return problems


def seconds_by_sentence(recorded: Mapping[str, RecordedDuration]) -> dict[str, float]:
    return {key: entry.seconds for key, entry in recorded.items()}
