"""Makes the voice of the video from the transcript.

The `tts` command of HyperFrames says each sentence into a file of its own,
so the build knows where each sentence starts with no transcription. The
voice of a sentence is in the cache under the hash of what made it, and the
build makes it one time.
"""

from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path
from typing import Mapping

import numpy
import soundfile

from tools import hyperframes, paths
from tools.timing import FRAMES_PER_SECOND, Timeline, sentence_key
from tools.transcript import Transcript
from tools.voice_durations import RecordedDuration, spoken_sentences

# The `tts` runs at the same time. One run loads the model and uses several cores.
PARALLEL_SYNTHESES = 4


class OtherVoice(Exception):
    """This machine makes another voice than the one that the record holds."""


@dataclass(frozen=True)
class SentenceVoice:
    key: str
    spoken_sha256: str
    file: Path
    seconds: float


def make_sentence_voices(transcript: Transcript, voice_tools_identity: str) -> list[SentenceVoice]:
    """The voice of each sentence in order, from the cache or from the model."""
    wanted = list(spoken_sentences(transcript, voice_tools_identity))
    paths.VOICE_CACHE.mkdir(parents=True, exist_ok=True)

    def make(entry: tuple[str, str, str]) -> SentenceVoice:
        key, spoken, sha256 = entry
        file = paths.VOICE_CACHE / f"{sha256}.wav"
        if not file.is_file():
            _synthesize(spoken, transcript, file)
        return SentenceVoice(key=key, spoken_sha256=sha256, file=file, seconds=_seconds_of(file))

    with ThreadPoolExecutor(max_workers=PARALLEL_SYNTHESES) as pool:
        return list(pool.map(make, wanted))


def record_of(
    voices: list[SentenceVoice], recorded: Mapping[str, RecordedDuration]
) -> dict[str, RecordedDuration]:
    """The record that agrees with `voices`.

    A sentence whose record has the hash of its voice keeps its recorded
    duration, so the frames of the video are the same on each machine.
    """
    record: dict[str, RecordedDuration] = {}
    for voice in voices:
        known = recorded.get(voice.key)
        if known is None or known.spoken_sha256 != voice.spoken_sha256:
            record[voice.key] = RecordedDuration(seconds=voice.seconds, spoken_sha256=voice.spoken_sha256)
            continue
        if abs(known.seconds - voice.seconds) > 1 / FRAMES_PER_SECOND:
            raise OtherVoice(
                f"The voice of `{voice.key}` is {voice.seconds} s on this machine, and voice_durations.json "
                f"records {known.seconds} s for the same text and the same tools."
            )
        record[voice.key] = known
    return record


def assemble_voice_track(
    transcript: Transcript, timeline: Timeline, voices: list[SentenceVoice], destination: Path
) -> None:
    """Write one audio file of the length of the video, with each sentence at its start."""
    file_of = {voice.key: voice.file for voice in voices}
    sample_rate = soundfile.info(str(voices[0].file)).samplerate
    samples_per_frame = sample_rate // FRAMES_PER_SECOND
    track = numpy.zeros(timeline.frames * samples_per_frame, dtype=numpy.int16)
    for segment, slot in zip(transcript.segments, timeline.segments, strict=True):
        for sentence in segment.voice:
            samples, rate = soundfile.read(str(file_of[sentence_key(segment.id, sentence.id)]), dtype="int16")
            if rate != sample_rate or samples.ndim != 1:
                raise ValueError(f"the voice of `{sentence.id}` is not one channel at {sample_rate} Hz")
            start = (slot.start_frame + slot.sentence(sentence.id).start_frame) * samples_per_frame
            track[start : start + len(samples)] = samples[: len(track) - start]
    destination.parent.mkdir(parents=True, exist_ok=True)
    soundfile.write(str(destination), track, sample_rate, subtype="PCM_16")


def _synthesize(spoken: str, transcript: Transcript, destination: Path) -> None:
    partial = destination.with_name(destination.stem + ".part.wav")
    hyperframes.output_of(
        [
            "tts",
            spoken,
            "--voice",
            transcript.voice.id,
            "--speed",
            str(transcript.voice.speed),
            "--lang",
            transcript.voice.language,
            "--output",
            str(partial),
            "--json",
        ]
    )
    partial.replace(destination)


def _seconds_of(file: Path) -> float:
    info = soundfile.info(str(file))
    return round(info.frames / info.samplerate, 4)
