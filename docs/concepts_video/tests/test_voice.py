"""The record and the track of the voice."""

from pathlib import Path

import numpy
import pytest
import soundfile

from tests.factories import segment_table, transcript_of
from tools.timing import lay_out
from tools.voice import OtherVoice, SentenceVoice, assemble_voice_track, record_of
from tools.voice_durations import RecordedDuration

SAMPLE_RATE = 24000


def voice_file(folder: Path, name: str, seconds: float, level: int) -> Path:
    file = folder / f"{name}.wav"
    soundfile.write(str(file), numpy.full(round(seconds * SAMPLE_RATE), level, dtype=numpy.int16), SAMPLE_RATE, subtype="PCM_16")
    return file


def sentence_voice(key: str, seconds: float, sha256: str = "a" * 64, file: Path = Path("unused.wav")) -> SentenceVoice:
    return SentenceVoice(key=key, spoken_sha256=sha256, file=file, seconds=seconds)


def test_a_new_voice_gives_a_new_entry_of_the_record() -> None:
    assert record_of([sentence_voice("opening/only", 2.5)], {}) == {
        "opening/only": RecordedDuration(seconds=2.5, spoken_sha256="a" * 64)
    }


def test_a_voice_of_another_text_replaces_the_entry_of_the_record() -> None:
    recorded = {"opening/only": RecordedDuration(seconds=9.0, spoken_sha256="b" * 64)}

    assert record_of([sentence_voice("opening/only", 2.5)], recorded)["opening/only"].seconds == 2.5


def test_the_same_voice_keeps_its_recorded_duration() -> None:
    recorded = {"opening/only": RecordedDuration(seconds=2.5, spoken_sha256="a" * 64)}

    assert record_of([sentence_voice("opening/only", 2.52)], recorded) == recorded


def test_another_duration_for_the_same_text_and_tools_is_refused() -> None:
    recorded = {"opening/only": RecordedDuration(seconds=2.5, spoken_sha256="a" * 64)}

    with pytest.raises(OtherVoice, match="opening/only"):
        record_of([sentence_voice("opening/only", 2.6)], recorded)


def test_the_track_has_each_sentence_at_its_start_and_the_length_of_the_video(tmp_path: Path) -> None:
    transcript = transcript_of(
        [segment_table(sentences=[{"id": "first", "text": "One."}, {"id": "second", "text": "Two."}])]
    )
    voices = [
        sentence_voice("opening/first", 1.0, file=voice_file(tmp_path, "first", 1.0, level=100)),
        sentence_voice("opening/second", 0.5, file=voice_file(tmp_path, "second", 0.5, level=200)),
    ]
    timeline = lay_out(transcript, {"opening/first": 1.0, "opening/second": 0.5})

    assemble_voice_track(transcript, timeline, voices, tmp_path / "track" / "voice.wav")

    track, rate = soundfile.read(str(tmp_path / "track" / "voice.wav"), dtype="int16")
    second = lambda seconds: round(seconds * SAMPLE_RATE)
    assert rate == SAMPLE_RATE
    assert len(track) == second(timeline.duration)
    assert set(track[: second(1.0)]) == {0}
    assert set(track[second(1.0) : second(2.0)]) == {100}
    assert set(track[second(2.0) : second(2.5)]) == {0}
    assert set(track[second(2.5) : second(3.0)]) == {200}
    assert set(track[second(3.0) :]) == {0}
