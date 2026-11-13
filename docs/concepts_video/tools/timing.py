"""When each sentence and each segment of the video starts and ends.

The voice is the clock of the video. Each sentence of the voice has a duration,
which `voice_durations.json` records. The start of each sentence and of each
segment comes from these durations, from the silences of the transcript and
from the duration of the splash screen, in full frames, so the same durations
always give the same frames.
"""

from dataclasses import dataclass
import math
from typing import Mapping

from tools.transcript import Segment, Transcript, Voice

FRAMES_PER_SECOND = 30


class MissingVoiceDuration(Exception):
    """A sentence of the transcript has no duration."""


def sentence_key(segment_id: str, sentence_id: str) -> str:
    """The name of a sentence in the full video."""
    return f"{segment_id}/{sentence_id}"


def frames_up(seconds: float) -> int:
    """The smallest number of full frames that holds `seconds`."""
    return math.ceil(round(seconds * FRAMES_PER_SECOND, 6))


def seconds_of(frames: int) -> float:
    return round(frames / FRAMES_PER_SECOND, 4)


@dataclass(frozen=True)
class SentenceSlot:
    id: str
    # From the start of the segment, in frames.
    start_frame: int
    end_frame: int

    @property
    def start(self) -> float:
        return seconds_of(self.start_frame)

    @property
    def end(self) -> float:
        return seconds_of(self.end_frame)


@dataclass(frozen=True)
class SegmentSlot:
    id: str
    # From the start of the video, in frames.
    start_frame: int
    frames: int
    sentences: tuple[SentenceSlot, ...]

    @property
    def start(self) -> float:
        return seconds_of(self.start_frame)

    @property
    def duration(self) -> float:
        return seconds_of(self.frames)

    def sentence(self, sentence_id: str) -> SentenceSlot:
        for slot in self.sentences:
            if slot.id == sentence_id:
                return slot
        raise KeyError(sentence_id)


@dataclass(frozen=True)
class Timeline:
    # The frames of the splash screen, which shows before the first segment.
    # A video with no splash screen has 0.
    splash_frames: int
    segments: tuple[SegmentSlot, ...]

    @property
    def frames(self) -> int:
        return self.splash_frames + sum(segment.frames for segment in self.segments)

    @property
    def duration(self) -> float:
        return seconds_of(self.frames)

    @property
    def splash_duration(self) -> float:
        return seconds_of(self.splash_frames)

    @property
    def voice_start(self) -> float:
        """The start of the first sentence of the video, from the start of the video."""
        first = self.segments[0]
        return seconds_of(first.start_frame + first.sentences[0].start_frame)

    @property
    def voice_end(self) -> float:
        """The end of the last sentence of the video, from the start of the video."""
        last = self.segments[-1]
        return seconds_of(last.start_frame + last.sentences[-1].end_frame)

    def segment(self, segment_id: str) -> SegmentSlot:
        for slot in self.segments:
            if slot.id == segment_id:
                return slot
        raise KeyError(segment_id)


def lay_out(transcript: Transcript, voice_seconds: Mapping[str, float]) -> Timeline:
    """The timeline of the video.

    `voice_seconds` gives the duration of the voice of each sentence, by its
    `sentence_key`.
    """
    splash_frames = 0 if transcript.splash is None else frames_up(transcript.splash.seconds)
    segments: list[SegmentSlot] = []
    start_frame = splash_frames
    for segment in transcript.segments:
        slot = _lay_out_segment(segment, transcript.voice, voice_seconds, start_frame)
        segments.append(slot)
        start_frame += slot.frames
    return Timeline(splash_frames=splash_frames, segments=tuple(segments))


def _lay_out_segment(
    segment: Segment, voice: Voice, voice_seconds: Mapping[str, float], start_frame: int
) -> SegmentSlot:
    sentences: list[SentenceSlot] = []
    cursor = frames_up(voice.lead_in_seconds)
    for index, sentence in enumerate(segment.voice):
        key = sentence_key(segment.id, sentence.id)
        if key not in voice_seconds:
            raise MissingVoiceDuration(key)
        end = cursor + frames_up(voice_seconds[key])
        sentences.append(SentenceSlot(id=sentence.id, start_frame=cursor, end_frame=end))
        is_last = index == len(segment.voice) - 1
        usual_silence = voice.tail_seconds if is_last else voice.sentence_gap_seconds
        silence = usual_silence if sentence.pause_after_seconds is None else sentence.pause_after_seconds
        cursor = end + frames_up(silence)
    return SegmentSlot(id=segment.id, start_frame=start_frame, frames=cursor, sentences=tuple(sentences))
