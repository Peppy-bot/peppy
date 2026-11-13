"""The transcript test: the files of the video project against the docs and against each other."""

from pathlib import Path

import pytest

from tools import checks, compiler, paths
from tools.footage import read_footage_list
from tools.pins import read_pins, voice_tools_identity
from tools.project import recorded_timeline
from tools.transcript import Transcript, read_transcript
from tools.voice_durations import out_of_date_problems, read_voice_durations

# The video is 9 minutes long at most.
MAX_VIDEO_SECONDS = 9 * 60


@pytest.fixture(scope="module")
def transcript() -> Transcript:
    return read_transcript(paths.TRANSCRIPT)


def test_each_named_section_exists_and_has_the_text_of_its_hash(transcript: Transcript) -> None:
    assert checks.source_problems(transcript, paths.DOCS_ROOT) == []


def test_each_technical_name_of_a_segment_is_in_the_sections_that_the_segment_names(transcript: Transcript) -> None:
    assert checks.technical_name_problems(transcript, paths.DOCS_ROOT) == []


def test_no_sentence_of_the_voice_is_longer_than_the_limit_of_asd_ste100(transcript: Transcript) -> None:
    assert checks.sentence_length_problems(transcript) == []


def test_the_record_of_the_voice_durations_agrees_with_the_transcript(transcript: Transcript) -> None:
    recorded = read_voice_durations(paths.VOICE_DURATIONS)

    assert out_of_date_problems(transcript, recorded, voice_tools_identity(read_pins(paths.PINS))) == []


def test_the_video_is_not_longer_than_its_limit(transcript: Transcript) -> None:
    assert recorded_timeline(transcript, read_pins(paths.PINS)).duration <= MAX_VIDEO_SECONDS


@pytest.mark.parametrize("template", compiler.segment_templates() + [paths.COMPOSITIONS / compiler.ROOT_TEMPLATE], ids=lambda path: path.name)
def test_a_composition_shows_no_text_of_its_own(template: Path) -> None:
    literal_text = compiler.literal_text_of_template(template.read_text(encoding="utf-8"))

    assert [word for word in literal_text.split() if any(character.isalpha() for character in word)] == []


def test_the_compositions_compile_with_each_text_of_the_transcript_and_the_listed_footage_only(
    transcript: Transcript, tmp_path: Path
) -> None:
    timeline = recorded_timeline(transcript, read_pins(paths.PINS))

    compiler.compile_project(transcript, timeline, read_footage_list(paths.FOOTAGE_LIST), tmp_path / "project")

    compiled = sorted(path.stem for path in (tmp_path / "project" / "compositions").iterdir())
    assert compiled == sorted(segment.id for segment in transcript.segments)
