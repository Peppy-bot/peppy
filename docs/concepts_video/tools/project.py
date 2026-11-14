"""The steps that the build, the snapshot and the lint have in common."""

from pathlib import Path

from tools import compiler, hyperframes, music, paths, voice
from tools.footage import FootageFile, read_footage_list
from tools.pins import Pins, read_pins, voice_tools_identity
from tools.timing import FRAMES_PER_SECOND, Timeline, lay_out
from tools.transcript import Transcript, read_transcript
from tools.voice_durations import (
    RecordedDuration,
    out_of_date_problems,
    read_voice_durations,
    seconds_by_sentence,
)


class OutOfDate(Exception):
    """`voice_durations.json` does not agree with the transcript."""


def read_project() -> tuple[Transcript, Pins, tuple[FootageFile, ...]]:
    return read_transcript(paths.TRANSCRIPT), read_pins(paths.PINS), read_footage_list(paths.FOOTAGE_LIST)


def part_of(transcript: Transcript, part_id: str) -> Transcript:
    """The transcript of the video of one part. Stop the command when the video has no such part."""
    if part_id not in transcript.part_ids:
        raise SystemExit(f"The video has no part `{part_id}`. Its parts are: {', '.join(transcript.part_ids)}.")
    return transcript.part(part_id)


def recorded_timeline(transcript: Transcript, pins: Pins) -> Timeline:
    """The timeline that `voice_durations.json` gives. Refuse a record that is out of date."""
    recorded = read_voice_durations(paths.VOICE_DURATIONS)
    problems = out_of_date_problems(transcript, recorded, voice_tools_identity(pins))
    if problems:
        raise OutOfDate("\n".join(problems + ["Run `pixi run build`, which makes the voice and writes the record."]))
    return timeline_of(transcript, recorded)


def timeline_of(transcript: Transcript, recorded: dict[str, RecordedDuration]) -> Timeline:
    return lay_out(transcript, seconds_by_sentence(recorded))


def compile_with_media(
    transcript: Transcript,
    record: dict[str, RecordedDuration],
    voices: list[voice.SentenceVoice],
    footage: tuple[FootageFile, ...],
    destination: Path,
) -> Timeline:
    """Compile the video of `transcript` into `destination`, and lint it.

    The compiled project has the voice, the music, the 3D model of the robot
    and the footage. The model and the footage must be in the cache.
    """
    timeline = timeline_of(transcript, record)
    compiler.compile_project(transcript, timeline, footage, destination)
    compiler.link_robot_model(paths.ROBOT_MODEL, destination)
    compiler.link_footage(footage, destination)
    voice.assemble_voice_track(transcript, timeline, voices, destination / compiler.VOICE_TRACK)
    music.write_music(timeline.frames / FRAMES_PER_SECOND, destination / compiler.MUSIC_TRACK)
    hyperframes.lint(destination, media_are_present=True)
    return timeline


def compile_and_lint_without_media() -> None:
    """Compile the project from the files of the repository alone, and lint it.

    The voice, the music and the footage are not in the repository, so the
    lint reports them as absent, and this report is not a fault.
    """
    transcript, pins, footage = read_project()
    compiler.compile_project(transcript, recorded_timeline(transcript, pins), footage, paths.COMPILED_PROJECT)
    hyperframes.lint(paths.COMPILED_PROJECT, media_are_present=False)

