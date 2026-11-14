"""The record of the duration of the voice of each sentence."""

from pathlib import Path

from tests.factories import segment_table, transcript_of
from tools.voice_durations import (
    RecordedDuration,
    out_of_date_problems,
    read_voice_durations,
    seconds_by_sentence,
    spoken_sentences,
    write_voice_durations,
)

TOOLS = "the tools of the voice"


def record_of(transcript) -> dict[str, RecordedDuration]:
    return {key: RecordedDuration(seconds=2.0, spoken_sha256=sha256) for key, _, sha256 in spoken_sentences(transcript, TOOLS)}


def test_the_record_that_the_build_writes_is_the_record_that_it_reads(tmp_path: Path) -> None:
    recorded = {"opening/only": RecordedDuration(seconds=2.5, spoken_sha256="a" * 64)}

    write_voice_durations(tmp_path / "voice_durations.json", recorded)

    assert read_voice_durations(tmp_path / "voice_durations.json") == recorded
    assert seconds_by_sentence(recorded) == {"opening/only": 2.5}


def test_a_project_with_no_record_has_an_empty_one(tmp_path: Path) -> None:
    assert read_voice_durations(tmp_path / "voice_durations.json") == {}


def test_the_voice_says_the_pronunciation_of_a_written_word() -> None:
    transcript = transcript_of(
        [segment_table(sentences=[{"id": "file", "text": "The file is `peppy.json5`."}])],
        pronunciation={"peppy.json5": "peppy dot jason five"},
    )

    ((key, spoken, _),) = spoken_sentences(transcript, TOOLS)

    assert (key, spoken) == ("opening/file", "The file is peppy dot jason five.")


def test_a_record_that_agrees_with_the_transcript_has_no_problem() -> None:
    transcript = transcript_of()

    assert out_of_date_problems(transcript, record_of(transcript), TOOLS) == []


def test_a_sentence_with_no_duration_and_a_duration_with_no_sentence_are_problems() -> None:
    transcript = transcript_of([segment_table(sentences=[{"id": "new", "text": "A new sentence."}])])
    recorded = {"opening/old": RecordedDuration(seconds=2.0, spoken_sha256="a" * 64)}

    assert out_of_date_problems(transcript, recorded, TOOLS) == [
        "voice_durations.json has no duration for the sentence `opening/new`.",
        "voice_durations.json has a duration for `opening/old`, which is not a sentence.",
    ]


def test_a_change_of_the_text_of_the_voice_or_of_its_tools_makes_the_record_out_of_date() -> None:
    before = transcript_of([segment_table(sentences=[{"id": "only", "text": "One sentence."}])])
    after = transcript_of([segment_table(sentences=[{"id": "only", "text": "Another sentence."}])])
    problem = ["voice_durations.json has the duration of another voice for the sentence `opening/only`."]

    assert out_of_date_problems(after, record_of(before), TOOLS) == problem
    assert out_of_date_problems(before, record_of(before), "other tools") == problem


def test_a_record_in_another_order_than_the_transcript_is_a_problem() -> None:
    transcript = transcript_of(
        [segment_table(sentences=[{"id": "first", "text": "One."}, {"id": "second", "text": "Two."}])]
    )
    reversed_record = dict(reversed(record_of(transcript).items()))

    assert out_of_date_problems(transcript, reversed_record, TOOLS) == [
        "voice_durations.json does not have the sentences in the order of the transcript."
    ]
