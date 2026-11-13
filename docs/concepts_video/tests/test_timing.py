"""The timeline of the video."""

import pytest

from tests.factories import segment_table as segment
from tests.factories import transcript_of
from tools.timing import FRAMES_PER_SECOND, MissingVoiceDuration, frames_up, lay_out, sentence_key


def test_a_duration_takes_the_smallest_number_of_full_frames_that_holds_it() -> None:
    assert frames_up(1.0) == FRAMES_PER_SECOND
    assert frames_up(1.001) == FRAMES_PER_SECOND + 1
    assert frames_up(0.1) == 3


def test_each_sentence_starts_after_the_sentence_before_it_and_the_silence() -> None:
    transcript = transcript_of(
        [segment("opening", [{"id": "first", "text": "One."}, {"id": "second", "text": "Two."}])]
    )

    timeline = lay_out(transcript, {"opening/first": 2.0, "opening/second": 3.0})

    slot = timeline.segment("opening")
    assert (slot.sentence("first").start, slot.sentence("first").end) == (1.0, 3.0)
    assert (slot.sentence("second").start, slot.sentence("second").end) == (3.5, 6.5)
    assert slot.duration == 8.5


def test_a_pause_of_a_sentence_replaces_the_usual_silence() -> None:
    transcript = transcript_of(
        [
            segment(
                "opening",
                [
                    {"id": "first", "text": "One.", "pause_after_seconds": 4.0},
                    {"id": "second", "text": "Two.", "pause_after_seconds": 0.0},
                ],
            )
        ]
    )

    slot = lay_out(transcript, {"opening/first": 2.0, "opening/second": 3.0}).segment("opening")

    assert slot.sentence("second").start == 7.0
    assert slot.duration == 10.0


def test_each_segment_starts_at_the_end_of_the_segment_before_it() -> None:
    transcript = transcript_of(
        [segment("opening", [{"id": "only", "text": "One."}]), segment("node", [{"id": "only", "text": "Two."}])]
    )

    timeline = lay_out(transcript, {"opening/only": 2.0, "node/only": 1.0})

    assert timeline.segment("opening").start == 0.0
    assert timeline.segment("node").start == 5.0
    assert timeline.duration == 9.0


def test_a_sentence_with_no_duration_is_refused() -> None:
    transcript = transcript_of([segment("opening", [{"id": "only", "text": "One."}])])

    with pytest.raises(MissingVoiceDuration, match="opening/only"):
        lay_out(transcript, {})


def test_the_key_of_a_sentence_holds_its_segment() -> None:
    assert sentence_key("opening", "only") == "opening/only"
