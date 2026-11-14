"""The command that checks the layout of the compositions."""

from tools import layout
from tools.layout_rules import BOX, Part
from tools.timing import SegmentSlot, SentenceSlot


def test_the_check_samples_each_sentence_at_its_middle_at_its_end_and_at_the_end_of_its_silence() -> None:
    slot = SegmentSlot(
        id="node",
        start_frame=0,
        frames=300,
        sentences=(SentenceSlot("first", 30, 90), SentenceSlot("second", 120, 240)),
    )

    assert layout.sample_times(slot) == [2.0, 2.95, 3.95, 6.0, 7.95, 9.4]


def test_a_fault_has_the_time_of_each_frame_that_has_it() -> None:
    outside = Part(name="the card", kind=BOX, left=1500, top=400, right=1900, bottom=600, path=(0,))
    inside = Part(name="the card", kind=BOX, left=1000, top=400, right=1400, bottom=600, path=(0,))

    faults = layout.faults_with_their_times([(1.0, [outside]), (2.0, [inside]), (3.5, [outside])])

    assert faults == {"the card is 76 px outside the margins of the frame": [1.0, 3.5]}


def test_a_record_of_the_page_is_a_part_of_the_frame() -> None:
    record = {"name": "#node-camera", "kind": "box", "left": 96, "top": 200, "right": 396, "bottom": 296, "border": [3, 3, 3, 3], "path": [2, 0], "isOver": True, "opacity": 1}

    assert layout.part_of(record) == Part(
        name="#node-camera", kind=BOX, left=96, top=200, right=396, bottom=296, path=(2, 0), is_over=True, border=(3, 3, 3, 3)
    )
