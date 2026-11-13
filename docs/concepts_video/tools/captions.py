"""The captions and the chapters of the video, from the transcript and the timeline.

The captions are a SubRip file for the upload: each sentence of the voice
shows while the voice says it. The chapters are the list that the description
of the video on YouTube needs, with one chapter for each segment.
"""

import math

from tools import markup
from tools.timing import FRAMES_PER_SECOND, Timeline
from tools.transcript import Transcript

# The usual limits of a caption: two lines of 42 characters.
MAX_LINE_CHARACTERS = 42
MAX_CUE_CHARACTERS = 2 * MAX_LINE_CHARACTERS


def subrip_captions(transcript: Transcript, timeline: Timeline) -> str:
    cues: list[tuple[float, float, str]] = []
    for segment, slot in zip(transcript.segments, timeline.segments, strict=True):
        for sentence in segment.voice:
            times = slot.sentence(sentence.id)
            start = (slot.start_frame + times.start_frame) / FRAMES_PER_SECOND
            end = (slot.start_frame + times.end_frame) / FRAMES_PER_SECOND
            cues += cues_of_sentence(markup.without_marks(sentence.text), start, end)
    blocks = [
        f"{number}\n{_subrip_time(start)} --> {_subrip_time(end)}\n{text}\n"
        for number, (start, end, text) in enumerate(cues, start=1)
    ]
    return "\n".join(blocks)


def cues_of_sentence(text: str, start: float, end: float) -> list[tuple[float, float, str]]:
    """The cues of one sentence. A long sentence has more than one cue.

    Each cue has a part of the time of the sentence in proportion to its text.
    """
    chunks = split_evenly(text, math.ceil(len(text) / MAX_CUE_CHARACTERS))
    total_characters = sum(len(chunk) for chunk in chunks)
    cues: list[tuple[float, float, str]] = []
    cue_start = start
    for index, chunk in enumerate(chunks):
        is_last = index == len(chunks) - 1
        cue_end = end if is_last else cue_start + (end - start) * len(chunk) / total_characters
        lines = split_evenly(chunk, math.ceil(len(chunk) / MAX_LINE_CHARACTERS))
        cues.append((cue_start, cue_end, "\n".join(lines)))
        cue_start = cue_end
    return cues


def split_evenly(text: str, parts: int) -> list[str]:
    """`text` in `parts` runs of full words, each as near to the same length as possible."""
    words = text.split()
    if parts <= 1 or len(words) <= 1:
        return [" ".join(words)]
    target = len(text) / parts
    runs: list[list[str]] = [[]]
    for word in words:
        length = len(" ".join(runs[-1]))
        length_with_word = length + 1 + len(word)
        is_nearer_without_word = abs(length - target) <= abs(length_with_word - target)
        if runs[-1] and is_nearer_without_word and len(runs) < parts:
            runs.append([])
        runs[-1].append(word)
    return [" ".join(run) for run in runs]


def youtube_chapters(transcript: Transcript, timeline: Timeline) -> str:
    """One line for each segment: the time of its start and its chapter title.

    YouTube makes chapters only from a list that starts at 0:00. Thus the
    first chapter starts at the start of the video, and the splash screen is
    in it.
    """
    start_frames = [0] + [slot.start_frame for slot in timeline.segments[1:]]
    lines = [
        f"{_chapter_time(start_frame // FRAMES_PER_SECOND)} {segment.chapter}"
        for segment, start_frame in zip(transcript.segments, start_frames, strict=True)
    ]
    return "\n".join(lines) + "\n"


def _subrip_time(seconds: float) -> str:
    milliseconds = round(seconds * 1000)
    hours, rest = divmod(milliseconds, 3_600_000)
    minutes, rest = divmod(rest, 60_000)
    whole_seconds, milliseconds = divmod(rest, 1000)
    return f"{hours:02}:{minutes:02}:{whole_seconds:02},{milliseconds:03}"


def _chapter_time(seconds: int) -> str:
    minutes, whole_seconds = divmod(seconds, 60)
    return f"{minutes}:{whole_seconds:02}"
