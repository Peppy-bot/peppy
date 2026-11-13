"""The reader of the transcript file."""

import pytest

from tests.factories import segment_table, transcript_document, transcript_of
from tools.transcript import CodeBlock, TranscriptError, parse_transcript


def test_a_segment_holds_its_sources_its_sentences_and_its_screen_texts() -> None:
    transcript = transcript_of(
        [
            segment_table(
                "node",
                [{"id": "first", "text": "A node is one program.", "pause_after_seconds": 2}],
                screen={"cards": {"node": {"term": "Node"}}, "steps": ["One", "Two"]},
            )
        ]
    )

    segment = transcript.segment("node")
    assert segment.sources[0].section == "A section"
    assert segment.voice[0].pause_after_seconds == 2.0
    assert dict(segment.screen_leaves()) == {
        "screen.cards.node.term": "Node",
        "screen.steps[0]": "One",
        "screen.steps[1]": "Two",
    }


def test_a_table_with_a_code_text_is_a_code_block() -> None:
    screen = {"file": {"language": "json5", "code": "\n{\n  name: 1,\n}\n", "example_names": ["name"]}}

    block = transcript_of([segment_table(screen=screen)]).segments[0].screen["file"]

    assert block == CodeBlock(language="json5", code="{\n  name: 1,\n}", example_names=("name",))
    assert block.lines == ["{", "  name: 1,", "}"]


def test_a_table_that_holds_a_code_block_is_not_a_code_block() -> None:
    screen = {"test": {"label": "One test", "code": {"language": "python", "code": "assert True"}}}

    leaves = dict(transcript_of([segment_table(screen=screen)]).segments[0].screen_leaves())

    assert leaves["screen.test.label"] == "One test"
    assert isinstance(leaves["screen.test.code"], CodeBlock)


@pytest.mark.parametrize(
    ("change", "reason"),
    [
        ({"id": "Opening"}, "an id has only lower-case letters"),
        ({"voice": []}, "it has no sentence of the voice"),
        ({"source": []}, "it names no section of the docs"),
        ({"source": [{"page": "p.mdx", "section": "S", "sha256": "abc"}]}, "is not a SHA-256"),
        ({"voice": [{"id": "a", "text": "One."}, {"id": "a", "text": "Two."}]}, "two entries have the id `a`"),
        ({"voice": [{"id": "a", "text": "One.", "pause_after_seconds": -1}]}, "is not a number of seconds"),
        ({"screen": {"title": "  "}}, "the text is blank"),
        ({"screen": {"count": 3}}, "it is not a text, a list, a table or a code block"),
        ({"screen": {"file": {"language": "rust", "code": "fn main() {}"}}}, "`language` is `rust`"),
        ({"screen": {"file": {"language": "sh", "code": "ls", "title": "A"}}}, "a code block has no key"),
    ],
)
def test_a_segment_in_another_form_is_refused(change: dict, reason: str) -> None:
    with pytest.raises(TranscriptError, match=reason):
        transcript_of([segment_table(**change)])


def test_two_segments_with_one_id_are_refused() -> None:
    with pytest.raises(TranscriptError, match="two entries have the id `opening`"):
        transcript_of([segment_table("opening"), segment_table("opening")])


def test_a_transcript_with_no_segment_is_refused() -> None:
    with pytest.raises(TranscriptError, match="it has no segment"):
        transcript_of([])


def test_a_voice_setting_that_is_not_above_zero_is_refused() -> None:
    document = transcript_document()
    document["voice"]["speed"] = 0

    with pytest.raises(TranscriptError, match="`speed` is not a number above zero"):
        parse_transcript(document)


def test_a_segment_has_the_number_of_its_place_in_the_video() -> None:
    transcript = transcript_of([segment_table("opening"), segment_table("node")])

    assert [(segment.id, segment.number) for segment in transcript.segments] == [("opening", 1), ("node", 2)]
