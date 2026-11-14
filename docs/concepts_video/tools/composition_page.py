"""The page of one composition in the pinned Chrome, for the checks of the compositions.

A check compiles the video of one segment, writes a page that holds the
composition alone with the styles and the scripts of the compiled project,
and gives the page to a probe: a Node script that opens it in the pinned
Chrome and prints what it reads as JSON.
"""

import argparse
from contextlib import contextmanager
from dataclasses import dataclass
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import re
import subprocess
import threading
from typing import Iterator

from tools import footage, paths, pins, project, voice
from tools.timing import SegmentSlot
from tools.voice_durations import read_voice_durations

TEMPLATE = re.compile(r"<template>(.*)</template>", re.DOTALL)

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


@dataclass(frozen=True)
class CompiledSegment:
    """The compiled video of one segment, and what a probe needs to read its composition."""

    id: str
    folder: Path
    slot: SegmentSlot
    chrome: Path

    def probe(self, script: Path, argument: str) -> object:
        """What the probe `script` prints for the composition of the segment, with `argument` as its last argument."""
        page = write_probe_page(self.folder, self.id)
        with served(self.folder) as address:
            completed = subprocess.run(
                ["node", str(script), str(self.chrome), f"{address}/{page.name}", self.id, argument],
                cwd=paths.PROJECT_ROOT,
                check=True,
                capture_output=True,
                text=True,
            )
        return json.loads(completed.stdout)


def segment_argument(description: str) -> str | None:
    """The segment that the command line of a check names, or None when it names no segment."""
    parser = argparse.ArgumentParser(description=description)
    parser.add_argument("segment", nargs="?", help="check this segment only, for example `opening`")
    return parser.parse_args().segment


def compiled_segments(segment: str | None) -> Iterator[CompiledSegment]:
    """Compile the video of each segment, or of `segment` only, with the voice and the pinned tools of the project."""
    transcript, pinned, footage_list = project.read_project()
    segment_ids = [each.id for each in transcript.segments]
    if segment is not None and segment not in segment_ids:
        raise SystemExit(f"The video has no segment `{segment}`. Its segments are: {', '.join(segment_ids)}.")

    chrome = pins.ensure_chrome(pinned)
    pins.ensure_robot_model(pinned)
    footage.fetch_absent(footage_list)
    voices = voice.make_sentence_voices(transcript, pins.voice_tools_identity(pinned))
    record = voice.record_of(voices, read_voice_durations(paths.VOICE_DURATIONS))

    for segment_id in segment_ids if segment is None else [segment]:
        folder = paths.BUILD / "parts" / segment_id
        timeline = project.compile_with_media(transcript.part(segment_id), record, voices, footage_list, folder)
        yield CompiledSegment(id=segment_id, folder=folder, slot=timeline.segments[0], chrome=chrome)


def write_probe_page(compiled: Path, segment_id: str) -> Path:
    """Write the page of one composition alone, with the styles and the scripts of the compiled project."""
    composition = (compiled / "compositions" / f"{segment_id}.html").read_text(encoding="utf-8")
    page = compiled / f"probe_{segment_id}.html"
    page.write_text(PROBE_PAGE.format(composition=composition_content(composition)), encoding="utf-8")
    return page


def composition_content(composition: str) -> str:
    """What the template element of a compiled composition holds."""
    found = TEMPLATE.search(composition)
    if found is None:
        raise ValueError("the compiled composition has no template element")
    return found.group(1)


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
