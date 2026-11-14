"""The command that checks the motion of the compositions."""

from tools import motion
from tools.layout_rules import TEXT
from tools.motion_rules import State, Track


def test_a_record_of_the_page_is_a_track_with_one_state_for_each_change() -> None:
    record = {
        "name": "#launchers-free-3 span",
        "kind": "text",
        "path": [2, 0, 1],
        "isOver": False,
        "border": [0, 0, 0, 0],
        "words": 5,
        "states": [[943, 0.61, 440.5, 600, 800, 640], [951, 0, 0, 0, 0, 0]],
    }

    assert motion.track_of(record) == Track(
        name="#launchers-free-3 span",
        kind=TEXT,
        path=(2, 0, 1),
        states=(State(943, 0.61, 440.5, 600, 800, 640), State(951, 0, 0, 0, 0, 0)),
        words=5,
    )
