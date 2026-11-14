"""Checks the motion of the compositions: `pixi run motion [segment id]`.

The command compiles the video of each segment, or of the one that it names,
and opens the composition in the pinned Chrome. At each frame of the
segment, it reads how much each box and each text shows, and its place, and
applies the rules of `tools.motion_rules` to them. It prints each fault, and
it fails when it finds one. Use it after each change of a composition, and
after each change of the voice: the times of a composition come from the
voice.

The command reads the parts that `pixi run layout` reads. In a composition,
`data-layout="free"` takes a part out of the two checks, for a decoration
and for a message in motion.
"""

from pathlib import Path

from tools.composition_page import CompiledSegment, compiled_segments, segment_argument
from tools.motion_rules import State, Track, motion_faults

PROBE = Path(__file__).resolve().parent / "motion_probe.mjs"


def main() -> None:
    fault_count = 0
    for segment in compiled_segments(segment_argument("Check the motion of the compositions of the concepts video.")):
        tracks = read_tracks(segment)
        faults = motion_faults(tracks, segment.slot.frames)
        fault_count += len(faults)
        print(f"Segment `{segment.id}`: {len(tracks)} parts, {len(faults)} faults.")
        for fault in faults:
            print(f"  {fault}")
    if fault_count > 0:
        raise SystemExit(f"The motion check found {fault_count} faults.")
    print("The motion check found no fault.")


def read_tracks(segment: CompiledSegment) -> list[Track]:
    """The track of each part that the composition of a compiled segment shows."""
    return [track_of(record) for record in segment.probe(PROBE, str(segment.slot.frames))]


def track_of(record: dict) -> Track:
    return Track(
        name=record["name"],
        kind=record["kind"],
        path=tuple(record["path"]),
        states=tuple(State(*state) for state in record["states"]),
        words=record["words"],
        is_over=record["isOver"],
        border=tuple(record["border"]),
    )


if __name__ == "__main__":
    main()
