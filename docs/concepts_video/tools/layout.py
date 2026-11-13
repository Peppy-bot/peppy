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

import argparse
from contextlib import contextmanager
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import re
import subprocess
import threading
from typing import Iterator

from tools import footage, paths, pins, project, voice
from tools.layout_rules import Part, frame_faults
from tools.timing import SegmentSlot
from tools.voice_durations import read_voice_durations

PROBE = Path(__file__).resolve().parent / "layout_probe.mjs"
TEMPLATE = re.compile(r"<template>(.*)</template>", re.DOTALL)
# The content of a segment goes out during this time before the end of the segment.
EXIT_SECONDS = 0.6
# A sample this time before the end of a sentence or before the start of the
# next one shows the parts of the sentence at rest.
REST_SECONDS = 0.05

PROBE_PAGE = """<!doctype html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <script src="lib/gsap.min.js"></script>
    <script src="lib/motion.js"></script>
    <script src="lib/robot_view.js"></script>
    <link rel="stylesheet" href="styles/video.css" />
    <script>
      window.__timelines = {{}};
    </script>
  </head>
  <body>
    {composition}
  </body>
</html>
"""


def main() -> None:
    parser = argparse.ArgumentParser(description="Check the layout of the compositions of the concepts video.")
    parser.add_argument("segment", nargs="?", help="check this segment only, for example `opening`")
    arguments = parser.parse_args()

    transcript, pinned, footage_list = project.read_project()
    segment_ids = [segment.id for segment in transcript.segments]
    if arguments.segment is not None and arguments.segment not in segment_ids:
        raise SystemExit(f"The video has no segment `{arguments.segment}`. Its segments are: {', '.join(segment_ids)}.")
    wanted = segment_ids if arguments.segment is None else [arguments.segment]

    chrome = pins.ensure_chrome(pinned)
    pins.ensure_robot_model(pinned)
    footage.fetch_absent(footage_list)
    voices = voice.make_sentence_voices(transcript, pins.voice_tools_identity(pinned))
    record = voice.record_of(voices, read_voice_durations(paths.VOICE_DURATIONS))

    fault_count = 0
    for segment_id in wanted:
        compiled = paths.BUILD / "parts" / segment_id
        timeline = project.compile_with_media(transcript.part(segment_id), record, voices, footage_list, compiled)
        frames = read_frames(compiled, segment_id, sample_times(timeline.segments[0]), chrome)
        faults = faults_with_their_times(frames)
        fault_count += len(faults)
        print(f"Segment `{segment_id}`: {len(frames)} frames, {len(faults)} faults.")
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


def read_frames(compiled: Path, segment_id: str, times: list[float], chrome: Path) -> list[tuple[float, list[Part]]]:
    """The parts that the composition of a compiled segment shows at each time."""
    page = write_probe_page(compiled, segment_id)
    with served(compiled) as address:
        url = f"{address}/{page.name}"
        completed = subprocess.run(
            ["node", str(PROBE), str(chrome), url, segment_id, ",".join(str(time) for time in times)],
            cwd=paths.PROJECT_ROOT,
            check=True,
            capture_output=True,
            text=True,
        )
    return [(frame["time"], [part_of(record) for record in frame["parts"]]) for frame in json.loads(completed.stdout)]


def write_probe_page(compiled: Path, segment_id: str) -> Path:
    """Write the page of one composition alone, with the styles and the scripts of the compiled project."""
    composition = (compiled / "compositions" / f"{segment_id}.html").read_text(encoding="utf-8")
    page = compiled / f"layout_{segment_id}.html"
    page.write_text(PROBE_PAGE.format(composition=composition_content(composition)), encoding="utf-8")
    return page


def composition_content(composition: str) -> str:
    """What the template element of a compiled composition holds."""
    found = TEMPLATE.search(composition)
    if found is None:
        raise ValueError("the compiled composition has no template element")
    return found.group(1)


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


@contextmanager
def served(folder: Path) -> Iterator[str]:
    """Serve the files of `folder` to this machine only, and give the address of the server."""
    handler = partial(QuietHandler, directory=str(folder))
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_address[1]}"
    finally:
        server.shutdown()
        server.server_close()


class QuietHandler(SimpleHTTPRequestHandler):
    """A handler that prints no line for a request."""

    def log_message(self, format: str, *arguments: object) -> None:
        return


if __name__ == "__main__":
    main()
