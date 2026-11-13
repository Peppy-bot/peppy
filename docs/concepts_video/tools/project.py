"""The steps that the build and the lint have in common."""

from tools import compiler, hyperframes, paths
from tools.footage import FootageFile, read_footage_list
from tools.pins import Pins, read_pins, voice_tools_identity
from tools.timing import Timeline, lay_out
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


def recorded_timeline(transcript: Transcript, pins: Pins) -> Timeline:
    """The timeline that `voice_durations.json` gives. Refuse a record that is out of date."""
    recorded = read_voice_durations(paths.VOICE_DURATIONS)
    problems = out_of_date_problems(transcript, recorded, voice_tools_identity(pins))
    if problems:
        raise OutOfDate("\n".join(problems + ["Run `pixi run build`, which makes the voice and writes the record."]))
    return timeline_of(transcript, recorded)


def timeline_of(transcript: Transcript, recorded: dict[str, RecordedDuration]) -> Timeline:
    return lay_out(transcript, seconds_by_sentence(recorded))


def compile_and_lint_without_media() -> None:
    """Compile the project from the files of the repository alone, and lint it.

    The voice and the footage are not in the repository, so the lint reports
    them as absent, and this report is not a fault.
    """
    transcript, pins, footage = read_project()
    compiler.compile_project(transcript, recorded_timeline(transcript, pins), footage, paths.COMPILED_PROJECT)
    hyperframes.lint(paths.COMPILED_PROJECT, media_are_present=False)

