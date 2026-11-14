"""The project test: the files of the video project against each other.

The check of the transcript against the docs is not here: it is the command
`pixi run docs-check`, which the release runs.
"""

from pathlib import Path

import pytest

from tools import checks, compiler, paths, upload
from tools.footage import read_footage_list
from tools.pins import read_pins, voice_tools_identity
from tools.project import recorded_timeline
from tools.transcript import SPLASH_ID, Transcript, read_transcript
from tools.voice_durations import out_of_date_problems, read_voice_durations

# The video is 10 minutes and 5 seconds long at most.
MAX_VIDEO_SECONDS = 10 * 60 + 5


@pytest.fixture(scope="module")
def transcript() -> Transcript:
    return read_transcript(paths.TRANSCRIPT)


def test_no_sentence_of_the_voice_is_longer_than_the_limit_of_asd_ste100(transcript: Transcript) -> None:
    assert checks.sentence_length_problems(transcript) == []


def test_the_record_of_the_voice_durations_agrees_with_the_transcript(transcript: Transcript) -> None:
    recorded = read_voice_durations(paths.VOICE_DURATIONS)

    assert out_of_date_problems(transcript, recorded, voice_tools_identity(read_pins(paths.PINS))) == []


def test_the_video_on_youtube_is_the_video_of_the_transcript() -> None:
    recorded = upload.read_upload(paths.UPLOAD)

    assert upload.out_of_date_problems(recorded, upload.content_sha256(paths.TRANSCRIPT)) == []


def test_the_video_is_not_longer_than_its_limit(transcript: Transcript) -> None:
    assert recorded_timeline(transcript, read_pins(paths.PINS)).duration <= MAX_VIDEO_SECONDS


@pytest.mark.parametrize("template", compiler.composition_templates(), ids=lambda path: path.name)
def test_a_composition_shows_no_text_of_its_own(template: Path) -> None:
    literal_text = compiler.literal_text_of_template(template.read_text(encoding="utf-8"))

    assert [word for word in literal_text.split() if any(character.isalpha() for character in word)] == []


def test_the_compositions_compile_with_each_text_of_the_transcript_and_the_listed_footage_only(
    transcript: Transcript, tmp_path: Path
) -> None:
    timeline = recorded_timeline(transcript, read_pins(paths.PINS))

    compiler.compile_project(transcript, timeline, read_footage_list(paths.FOOTAGE_LIST), tmp_path / "project")

    compiled = sorted(path.stem for path in (tmp_path / "project" / "compositions").iterdir())
    assert compiled == sorted([SPLASH_ID] + [segment.id for segment in transcript.segments])
