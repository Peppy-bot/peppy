"""The rules of the motion of a segment. The video has 30 frames in each second."""

from tools.layout_rules import BOX, TEXT
from tools.motion_rules import Showing, State, Track, motion_faults, showings

# The number of frames of the segment of each test: 10 seconds.
FRAMES = 300


def shown(frame: int, left: float = 200, top: float = 300, width: float = 400, height: float = 60, opacity: float = 1.0) -> State:
    return State(frame, opacity, left, top, left + width, top + height)


def gone(frame: int) -> State:
    return State(frame, 0.0, 0.0, 0.0, 0.0, 0.0)


def box(name: str, path: tuple[int, ...], *states: State, **more: object) -> Track:
    return Track(name=name, kind=BOX, path=path, states=states, **more)


def text(name: str, path: tuple[int, ...], words: int, *states: State) -> Track:
    return Track(name=name, kind=TEXT, path=path, states=states, words=words)


def test_a_part_that_stays_at_rest_long_enough_has_no_fault() -> None:
    card = box("the card", (0,), shown(30), gone(76))

    assert showings(card, FRAMES) == [Showing(start_frame=30, end_frame=76, rest_frames=45)]
    assert motion_faults([card], FRAMES) == []


def test_a_part_that_goes_out_too_soon_after_it_comes_in_is_a_fault() -> None:
    """The third line of a list comes in 0.1 seconds before its scene goes out."""
    line = box("line 3", (0,), shown(943), gone(946))

    assert motion_faults([line], 1500) == ["line 3 comes in at 31.43 s and is at rest for 0.07 s: it needs 1.50 s"]


def test_a_part_is_not_at_rest_while_it_changes() -> None:
    """A part that moves during 15 frames, then stays during 44 frames, is at rest for 1.47 seconds."""
    moves = [shown(30 + frame, left=140 + 4 * frame, opacity=0.25 + 0.05 * frame) for frame in range(15)]
    card = box("the card", (0,), *moves, shown(45), gone(90))

    assert showings(card, FRAMES) == [Showing(start_frame=30, end_frame=90, rest_frames=44)]
    assert motion_faults([card], FRAMES) == ["the card comes in at 1.00 s and is at rest for 1.47 s: it needs 1.50 s"]


def test_a_text_needs_the_time_to_read_each_of_its_words() -> None:
    """A text of 20 words needs 5.6 seconds: it has a fault at 5 seconds, and no fault at 6 seconds."""
    short = text("the definition", (0,), 20, shown(30), gone(181))
    long_enough = text("the definition", (0,), 20, shown(30), gone(211))

    assert motion_faults([short], FRAMES) == [
        "the definition comes in at 1.00 s and is at rest for 5.00 s: it needs 5.60 s"
    ]
    assert motion_faults([long_enough], FRAMES) == []


def test_a_part_that_goes_into_the_background_stays_on_the_screen() -> None:
    """A line of code that a composition dims after one half of a second is one showing."""
    line = text("line 1", (0,), 4, shown(30), shown(45, opacity=0.28), gone(200))

    assert showings(line, FRAMES) == [Showing(start_frame=30, end_frame=200, rest_frames=168)]
    assert motion_faults([line], FRAMES) == []


def test_each_time_that_a_part_comes_in_is_one_showing() -> None:
    """A status that comes back for one second after a long first showing has a fault at its second showing only."""
    status = text("the status", (0,), 1, shown(30), gone(150), shown(180), gone(210))

    assert showings(status, FRAMES) == [Showing(30, 150, 119), Showing(180, 210, 29)]
    assert motion_faults([status], FRAMES) == ["the status comes in at 6.00 s and is at rest for 0.97 s: it needs 1.50 s"]


def test_a_part_that_shows_until_the_end_of_the_segment_is_at_rest_until_the_end() -> None:
    card = box("the card", (0,), shown(250))

    assert showings(card, FRAMES) == [Showing(start_frame=250, end_frame=300, rest_frames=50)]


def test_a_part_inside_a_part_that_goes_out_too_soon_gives_no_second_fault() -> None:
    """A pill is a box and a text: the two give one fault. Its icon is inside it, and gives none."""
    pill = box("the pill", (0,), shown(30), gone(60))
    name = text("the pill", (0,), 1, shown(30, left=220, width=100), gone(60))
    icon = box("the icon", (0, 0), shown(30, left=204, width=12, height=12), gone(60))

    assert motion_faults([pill, name, icon], FRAMES) == ["the pill comes in at 1.00 s and is at rest for 0.97 s: it needs 1.50 s"]


def test_a_part_that_comes_in_over_a_part_that_still_shows_is_a_fault() -> None:
    """The title of a new scene comes in at 3.0 seconds over a node of the scene before it, which is gone at 3.2 seconds."""
    node = box("the node", (0, 0), shown(0, left=200, top=300, width=300, height=96), gone(96))
    title = text("the title", (1, 0), 3, shown(90, left=150, top=320, width=700, height=50))

    assert motion_faults([node, title], FRAMES) == [
        "1 pairs of parts show at the same place from 3.00 s to 3.20 s: the node and the title"
    ]


def test_a_part_that_comes_in_when_the_part_before_it_is_gone_is_not_a_fault() -> None:
    node = box("the node", (0, 0), shown(0, left=200, top=300, width=300, height=96), gone(90))
    title = text("the title", (1, 0), 3, shown(90, left=150, top=320, width=700, height=50))

    assert motion_faults([node, title], FRAMES) == []


def test_a_part_that_nearly_does_not_show_is_at_no_place() -> None:
    """The node is nearly gone when the title comes in: the two do not show together."""
    node = box("the node", (0, 0), shown(0, left=200, top=300, width=300, height=96), shown(90, left=200, top=300, width=300, height=96, opacity=0.15), gone(96))
    title = text("the title", (1, 0), 3, shown(90, left=150, top=320, width=700, height=50))

    assert motion_faults([node, title], FRAMES) == []


def test_a_change_of_scene_with_many_pairs_is_one_fault() -> None:
    """Five lines of the scene before stay under the title of the new scene from 3.0 to 3.2 seconds."""
    lines = [text(f"line {number}", (0, number), 1, shown(0, left=200, top=300, width=300, height=10 * number), gone(96)) for number in range(1, 6)]
    title = text("the title", (1, 0), 3, shown(90, left=150, top=300, width=700, height=8))

    assert motion_faults([*lines, title], FRAMES) == [
        "5 pairs of parts show at the same place from 3.00 s to 3.20 s:"
        " line 1 and the title; line 2 and the title; line 3 and the title; and 2 more"
    ]


def test_two_times_at_which_parts_overlap_are_two_faults() -> None:
    """A dot that is not out of the check crosses a label two times."""
    label = text("the label", (0,), 2, shown(0, left=400, top=300, width=200, height=40))
    dot = box("the dot", (1,), shown(60, left=380, top=310, width=40, height=20), gone(63), shown(150, left=580, top=310, width=40, height=20), gone(153))

    faults = motion_faults([label, dot], FRAMES)

    assert [fault for fault in faults if "same place" in fault] == [
        "1 pairs of parts show at the same place from 2.00 s to 2.10 s: the label and the dot",
        "1 pairs of parts show at the same place from 5.00 s to 5.10 s: the label and the dot",
    ]
