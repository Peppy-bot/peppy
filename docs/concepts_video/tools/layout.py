"""Checks the layout of the compositions: `pixi run layout [segment id]`.

The command compiles the video of each segment, or of the one that it names,
and opens the composition in the pinned Chrome. At some times of each
sentence of the voice, it reads the place of each box and of each text that
the page shows, and applies the rules of `tools.layout_rules` to them. It
prints each fault with the times at which the page shows it, and it fails
when it finds one. Use it after each change of a composition.

In a composition, `data-layout="free"` takes a part out of the check, for a
decoration and for a message in motion. `data-layout="over"` says that a part
is over other parts on purpose, as a tab on a border or a mark on an arrow.
"""

from pathlib import Path

from tools.composition_page import CompiledSegment, compiled_segments, segment_argument
from tools.layout_rules import Part, frame_faults
from tools.timing import SegmentSlot

PROBE = Path(__file__).resolve().parent / "layout_probe.mjs"
# The content of a segment goes out during this time before the end of the segment.
EXIT_SECONDS = 0.6
# A sample this time before the end of a sentence or before the start of the
# next one shows the parts of the sentence at rest.
REST_SECONDS = 0.05


def main() -> None:
    fault_count = 0
    for segment in compiled_segments(segment_argument("Check the layout of the compositions of the concepts video.")):
        frames = read_frames(segment, sample_times(segment.slot))
        faults = faults_with_their_times(frames)
        fault_count += len(faults)
        print(f"Segment `{segment.id}`: {len(frames)} frames, {len(faults)} faults.")
        for fault, times in faults.items():
            print(f"  {fault} (at {', '.join(f'{time:g} s' for time in times)})")
    if fault_count > 0:
        raise SystemExit(f"The layout check found {fault_count} faults.")
    print("The layout check found no fault.")


def sample_times(slot: SegmentSlot) -> list[float]:
    """The times of the check, in seconds from the start of the segment.

    For each sentence: its middle, its end, and the end of the silence after
    it. The last time is before the content of the segment goes out.
    """
    last = round(slot.duration - EXIT_SECONDS, 2)
    times: set[float] = {last}
    for index, sentence in enumerate(slot.sentences):
        is_last = index == len(slot.sentences) - 1
        next_start = slot.duration if is_last else slot.sentences[index + 1].start
        times.update(
            {
                round((sentence.start + sentence.end) / 2, 2),
                round(sentence.end - REST_SECONDS, 2),
                round(next_start - REST_SECONDS, 2),
            }
        )
    return sorted(time for time in times if time <= last)


def read_frames(segment: CompiledSegment, times: list[float]) -> list[tuple[float, list[Part]]]:
    """The parts that the composition of a compiled segment shows at each time."""
    frames = segment.probe(PROBE, ",".join(str(time) for time in times))
    return [(frame["time"], [part_of(record) for record in frame["parts"]]) for frame in frames]


def part_of(record: dict) -> Part:
    return Part(
        name=record["name"],
        kind=record["kind"],
        left=record["left"],
        top=record["top"],
        right=record["right"],
        bottom=record["bottom"],
        path=tuple(record["path"]),
        is_over=record["isOver"],
        border=tuple(record["border"]),
    )


def faults_with_their_times(frames: list[tuple[float, list[Part]]]) -> dict[str, list[float]]:
    """Each fault of the frames, with the times of the frames that have it."""
    faults: dict[str, list[float]] = {}
    for time, parts in frames:
        for fault in frame_faults(parts):
            faults.setdefault(fault, []).append(time)
    return faults


if __name__ == "__main__":
    main()
