"""The rules of the motion of one segment of the video.

A track is one part of a composition, a box or a text as in
`tools.layout_rules`, with its state at each frame of the segment: how much
the part shows, and its place. The rules find the faults that a person sees
as a motion that is too quick to follow:

- A part that comes in and goes out again before the viewer can see it, or
  before the viewer can read it.
- A part that shows at the place of another part that also shows, as a scene
  that comes in over the scene before it.

The rules read numbers only, so the same tracks always give the same result.
"""

from bisect import bisect_right
from dataclasses import dataclass

from tools.layout_rules import Part, overlapping_pairs
from tools.timing import FRAMES_PER_SECOND

# A part shows from this opacity. A part that a composition puts in the
# background stays above it.
SHOWN_OPACITY = 0.2
# The shortest time for which a part that comes in stays at rest.
MIN_REST_SECONDS = 1.5
# The time that the viewer needs to read one word of a text: the pace of the voice.
READ_SECONDS_PER_WORD = 0.28
# A fault of parts that overlap names this number of pairs.
NAMED_PAIRS = 3
# A difference of time that the eye does not see: one half of a frame.
TOLERANCE_SECONDS = 0.5 / FRAMES_PER_SECOND


@dataclass(frozen=True)
class State:
    """How a part shows from one frame until its next state."""

    frame: int
    opacity: float
    left: float
    top: float
    right: float
    bottom: float

    @property
    def is_shown(self) -> bool:
        return self.opacity >= SHOWN_OPACITY


@dataclass(frozen=True)
class Track:
    """One part of a composition, with one state for each frame at which the part changes."""

    name: str
    kind: str
    # The place of the element of the part in the page, as in `Part`.
    path: tuple[int, ...]
    states: tuple[State, ...]
    # The number of words of a text.
    words: int = 0
    is_over: bool = False
    border: tuple[float, float, float, float] = (0.0, 0.0, 0.0, 0.0)

    def is_inside(self, other: "Track") -> bool:
        """True when the element of this part is inside the element of `other`."""
        return len(self.path) > len(other.path) and self.path[: len(other.path)] == other.path

    def needed_rest_seconds(self) -> float:
        """The time for which the part must stay at rest each time it comes in."""
        return max(MIN_REST_SECONDS, self.words * READ_SECONDS_PER_WORD)


@dataclass(frozen=True)
class Showing:
    """One time that a part is on the screen: from the frame at which it comes in to the frame at which it is gone."""

    start_frame: int
    end_frame: int
    # The frames in which the part shows and is the same in the next frame.
    rest_frames: int

    @property
    def start(self) -> float:
        return self.start_frame / FRAMES_PER_SECOND

    @property
    def rest_seconds(self) -> float:
        return self.rest_frames / FRAMES_PER_SECOND

    def is_during(self, other: "Showing") -> bool:
        return self.start_frame < other.end_frame and other.start_frame < self.end_frame


def motion_faults(tracks: list[Track], frames: int) -> list[str]:
    """The faults of the motion of a segment of `frames` frames, as sentences for the person who corrects the composition."""
    return _short_showings(tracks, frames) + _parts_at_one_place(tracks, frames)


def showings(track: Track, frames: int) -> list[Showing]:
    """Each time that the part of `track` is on the screen, in a segment of `frames` frames."""
    found: list[Showing] = []
    start: int | None = None
    rest = 0
    for index, state in enumerate(track.states):
        is_last = index == len(track.states) - 1
        end = frames if is_last else track.states[index + 1].frame
        if not state.is_shown:
            if start is not None:
                found.append(Showing(start, state.frame, rest))
            start = None
            continue
        if start is None:
            start, rest = state.frame, 0
        # The last frame of a state is the frame before a change.
        rest += end - state.frame - (0 if is_last else 1)
    if start is not None:
        found.append(Showing(start, frames, rest))
    return found


def _short_showings(tracks: list[Track], frames: int) -> list[str]:
    """The parts that are at rest for too short a time when they come in.

    A part inside a part that has this fault at the same time has the same fault, and is not in the result.
    """
    short = [
        (track, showing)
        for track in tracks
        for showing in showings(track, frames)
        if showing.rest_seconds + TOLERANCE_SECONDS < track.needed_rest_seconds()
    ]
    faults = [
        f"{track.name} comes in at {showing.start:.2f} s and is at rest for {showing.rest_seconds:.2f} s:"
        f" it needs {track.needed_rest_seconds():.2f} s"
        for track, showing in short
        if not any(track.is_inside(other) and showing.is_during(other_showing) for other, other_showing in short)
    ]
    # An element that is a box and a text gives the same fault two times.
    return list(dict.fromkeys(faults))


def _parts_at_one_place(tracks: list[Track], frames: int) -> list[str]:
    """The times at which parts overlap while they show, at rest or in motion.

    One change of scene gives many pairs of parts that overlap at the same time: the result holds one fault for
    each time, with the number of the pairs and the first ones.
    """
    spans = sorted(_spans_of_overlaps(tracks, frames))
    faults: list[str] = []
    group: list[tuple[int, int, str, str]] = []
    for span in spans:
        if group and span[0] >= max(end for _, end, _, _ in group):
            faults.append(_fault_of_overlaps(group))
            group = []
        group.append(span)
    if group:
        faults.append(_fault_of_overlaps(group))
    return faults


def _fault_of_overlaps(group: list[tuple[int, int, str, str]]) -> str:
    start = min(start for start, _, _, _ in group) / FRAMES_PER_SECOND
    end = max(end for _, end, _, _ in group) / FRAMES_PER_SECOND
    named = "; ".join(f"{first} and {second}" for _, _, first, second in group[:NAMED_PAIRS])
    more = "" if len(group) <= NAMED_PAIRS else f"; and {len(group) - NAMED_PAIRS} more"
    return f"{len(group)} pairs of parts show at the same place from {start:.2f} s to {end:.2f} s: {named}{more}"


def _spans_of_overlaps(tracks: list[Track], frames: int) -> list[tuple[int, int, str, str]]:
    """Each time that two parts overlap while the two show: its first frame, the frame after its last one, and the names of the two parts."""
    frames_of_states = [[state.frame for state in track.states] for track in tracks]
    changes = sorted({frame for frames_of_track in frames_of_states for frame in frames_of_track})
    # The spans of each pair, by the kinds and the paths of its parts.
    spans: dict[tuple, list[tuple[int, int, str, str]]] = {}
    for number, frame in enumerate(changes):
        next_change = frames if number == len(changes) - 1 else changes[number + 1]
        for first, second, _, _ in overlapping_pairs(_parts_at(tracks, frames_of_states, frame)):
            of_pair = spans.setdefault((first.kind, first.path, second.kind, second.path), [])
            goes_on = bool(of_pair) and of_pair[-1][1] == frame
            start = of_pair.pop()[0] if goes_on else frame
            of_pair.append((start, next_change, first.name, second.name))
    return [span for of_pair in spans.values() for span in of_pair]


def _parts_at(tracks: list[Track], frames_of_states: list[list[int]], frame: int) -> list[Part]:
    """The parts that show at `frame`, each with its place at this frame."""
    parts: list[Part] = []
    for track, frames_of_track in zip(tracks, frames_of_states):
        index = bisect_right(frames_of_track, frame) - 1
        if index < 0 or not track.states[index].is_shown:
            continue
        state = track.states[index]
        parts.append(
            Part(
                name=track.name,
                kind=track.kind,
                left=state.left,
                top=state.top,
                right=state.right,
                bottom=state.bottom,
                path=track.path,
                is_over=track.is_over,
                border=track.border,
            )
        )
    return parts
