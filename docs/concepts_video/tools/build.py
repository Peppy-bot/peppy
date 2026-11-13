"""Builds the video from the files of the repository: `pixi run build`.

The build gets the pinned Chrome, the model of the voice and the footage,
makes the voice, compiles the compositions, renders the MP4 file, and writes
the captions and the chapters. It writes each output into `build/`.

`pixi run build <segment id>` builds the video of one segment only, into
`build/<segment id>.mp4`.
"""

import argparse
from dataclasses import replace
from pathlib import Path
import subprocess

from tools import captions, compiler, footage, hyperframes, paths, pins, project, voice
from tools.timing import FRAMES_PER_SECOND, Timeline
from tools.transcript import Transcript
from tools.voice_durations import RecordedDuration, read_voice_durations, write_voice_durations

# The render settings that change the file. `delivery` is the quality of
# HyperFrames for a final file. Chrome draws with its software renderer, so a
# machine with a graphics card and a machine with none give the same frames.
# The count of the workers is a part of the settings: each worker captures
# one part of the frames, and the bytes of the video change with the count.
# With no count, HyperFrames takes one from the processors of the machine and
# from the speed that it measures.
RENDER_OPTIONS = ["--quality", "delivery", "--no-browser-gpu", "--workers", "4", "--strict-all"]
VOICE_TRACK = paths.COMPILED_PROJECT / "assets" / "voice.wav"


def main() -> None:
    arguments = _parse_arguments()
    transcript, pinned, footage_list = project.read_project()
    if arguments.segment is not None and arguments.segment not in [segment.id for segment in transcript.segments]:
        raise SystemExit(f"The transcript has no segment `{arguments.segment}`.")

    _install_node_packages()
    chrome = pins.ensure_chrome(pinned)
    pins.ensure_voice_model(pinned)
    pins.ensure_robot_model(pinned)
    _fetch_footage(footage_list)

    voices = voice.make_sentence_voices(transcript, pins.voice_tools_identity(pinned))
    recorded = read_voice_durations(paths.VOICE_DURATIONS)
    record = voice.record_of(voices, recorded)
    if record != recorded:
        write_voice_durations(paths.VOICE_DURATIONS, record)
        print(f"The voice changed: {paths.VOICE_DURATIONS.name} has the new durations. Commit it.")

    if arguments.segment is not None:
        one_segment = replace(transcript, segments=(transcript.segment(arguments.segment),))
        _compile_and_render(one_segment, record, voices, footage_list, chrome, paths.BUILD / f"{arguments.segment}.mp4")
        return
    timeline = _compile_and_render(transcript, record, voices, footage_list, chrome, paths.VIDEO)
    paths.CAPTIONS.write_text(captions.subrip_captions(transcript, timeline), encoding="utf-8")
    paths.CHAPTERS.write_text(captions.youtube_chapters(transcript, timeline), encoding="utf-8")
    _report(transcript, timeline)


def _parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Build the concepts video.")
    parser.add_argument("segment", nargs="?", help="render the composition of this segment only")
    return parser.parse_args()


def _install_node_packages() -> None:
    """Install HyperFrames and GSAP at the versions of package-lock.json."""
    subprocess.run(["npm", "ci"], cwd=paths.PROJECT_ROOT, check=True)


def _fetch_footage(footage_list: tuple[footage.FootageFile, ...]) -> None:
    absent = [file for file in footage_list if not footage.is_in_cache(file)]
    if not absent:
        return
    client = footage.bucket_client()
    for file in absent:
        print(f"Getting the footage {file.name}")
        footage.fetch(file, client)


def _compile_and_render(
    transcript: Transcript,
    record: dict[str, RecordedDuration],
    voices: list[voice.SentenceVoice],
    footage_list: tuple[footage.FootageFile, ...],
    chrome: Path,
    output: Path,
) -> Timeline:
    """Compile the segments of `transcript` into one project, and render it into `output`."""
    timeline = project.timeline_of(transcript, record)
    compiler.compile_project(transcript, timeline, footage_list, paths.COMPILED_PROJECT)
    compiler.link_robot_model(paths.ROBOT_MODEL, paths.COMPILED_PROJECT)
    compiler.link_footage(footage_list, paths.COMPILED_PROJECT)
    voice.assemble_voice_track(transcript, timeline, voices, VOICE_TRACK)
    hyperframes.lint(paths.COMPILED_PROJECT, media_are_present=True)
    hyperframes.run(
        ["render", str(paths.COMPILED_PROJECT), *RENDER_OPTIONS, "--output", str(output)],
        extra_environment={"PRODUCER_HEADLESS_SHELL_PATH": str(chrome)},
    )
    return timeline


def _report(transcript: Transcript, timeline: Timeline) -> None:
    minutes, seconds = divmod(timeline.frames // FRAMES_PER_SECOND, 60)
    print(f"The video is {minutes} min {seconds} s long, in {len(transcript.segments)} chapters.")
    print(f"Video:    {paths.VIDEO}")
    print(f"Captions: {paths.CAPTIONS}")
    print(f"Chapters: {paths.CHAPTERS}")


if __name__ == "__main__":
    main()
