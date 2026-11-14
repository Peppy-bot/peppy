"""The reader of the transcript file."""

import pytest

from tests.factories import act_table, segment_table, splash_table, transcript_document, transcript_of
from tools.transcript import Chapter, CodeBlock, Splash, TranscriptError, parse_transcript


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


def test_a_transcript_with_no_splash_table_has_no_splash_screen() -> None:
    assert transcript_of().splash is None


def test_the_splash_screen_holds_its_duration_and_its_texts() -> None:
    transcript = transcript_of(splash=splash_table(3, name="Peppy", title="The concepts"))

    assert transcript.splash == Splash(seconds=3.0, screen={"name": "Peppy", "title": "The concepts"})


@pytest.mark.parametrize(
    ("splash", "reason"),
    [
        ("Peppy", "`splash` is not a table"),
        ({"seconds": 0, "screen": {"title": "A title"}}, "`seconds` is not a number above zero"),
        ({"seconds": 4}, "`screen` is not a table"),
        ({"seconds": 4, "screen": {"title": ["A title"]}}, "splash.screen.title: it is not a text"),
        ({"seconds": 4, "screen": {"title": "Run `peppy`"}}, "splash.screen.title: it holds a technical name"),
    ],
)
def test_a_splash_screen_in_another_form_is_refused(splash: object, reason: str) -> None:
    with pytest.raises(TranscriptError, match=reason):
        transcript_of(splash=splash)


def test_a_segment_with_the_id_of_the_splash_screen_is_refused() -> None:
    with pytest.raises(TranscriptError, match="a segment has the id `splash`"):
        transcript_of([segment_table("splash")])


def test_the_parts_of_a_video_are_its_splash_screen_and_its_segments() -> None:
    with_splash = transcript_of([segment_table("opening"), segment_table("node")], splash=splash_table())
    without_splash = transcript_of([segment_table("opening"), segment_table("node")])

    assert with_splash.part_ids == ["splash", "opening", "node"]
    assert without_splash.part_ids == ["opening", "node"]


def test_the_video_of_a_segment_has_this_segment_only_and_no_splash_screen() -> None:
    transcript = transcript_of([segment_table("opening"), segment_table("node")], splash=splash_table())

    part = transcript.part("node")

    assert part.splash is None
    assert [segment.id for segment in part.segments] == ["node"]
    assert part.segments[0].number == 2


def test_the_video_of_the_splash_screen_has_the_first_segment_after_it() -> None:
    transcript = transcript_of([segment_table("opening"), segment_table("node")], splash=splash_table())

    part = transcript.part("splash")

    assert part.splash == transcript.splash
    assert [segment.id for segment in part.segments] == ["opening"]


@pytest.mark.parametrize("part_id", ["absent", "splash"])
def test_a_part_that_the_video_does_not_have_is_refused(part_id: str) -> None:
    with pytest.raises(KeyError, match=part_id):
        transcript_of([segment_table("opening")]).part(part_id)


def test_an_act_holds_the_chapters_of_its_segments_in_order() -> None:
    transcript = transcript_of(
        [
            segment_table("opening"),
            segment_table("node", act="one_node", chapter="A node"),
            segment_table("file", act="one_node", chapter="A file"),
            segment_table("stack", act="stack", chapter="The stack"),
        ],
        act=[act_table("one_node", title="One node"), act_table("stack")],
    )

    first, second = transcript.acts
    assert (first.number, first.title, second.number) == (1, "One node", 2)
    assert first.chapters == (Chapter("node", 2, "A node"), Chapter("file", 3, "A file"))
    assert first.chapter_index("file") == 1
    assert transcript.act("stack").chapters == (Chapter("stack", 4, "The stack"),)
    assert transcript.segment("opening").act is None


def test_the_video_of_one_segment_keeps_the_chapters_of_each_act() -> None:
    transcript = transcript_of(
        [segment_table("node", act="one_node"), segment_table("file", act="one_node")], act=[act_table("one_node")]
    )

    assert transcript.part("file").acts == transcript.acts


@pytest.mark.parametrize(
    ("segments", "acts", "reason"),
    [
        ([segment_table("node", act="absent")], [act_table("one_node")], "the transcript has no such act"),
        ([segment_table("node")], [act_table("one_node")], "each act has segments"),
        (
            [segment_table("node", act="one_node"), segment_table("stack", act="stack"), segment_table("file", act="one_node")],
            [act_table("one_node"), act_table("stack")],
            "which follow each other",
        ),
        (
            [segment_table("stack", act="stack"), segment_table("node", act="one_node")],
            [act_table("one_node"), act_table("stack")],
            "in the order of the acts",
        ),
        ([segment_table("node", act="one_node")], [act_table("one_node"), act_table("one_node")], "two entries have the id `one_node`"),
        ([segment_table("node", act="one_node")], [act_table("one_node", question=" ")], "`question` is not a text"),
    ],
)
def test_acts_that_the_segments_do_not_fill_are_refused(segments: list[dict], acts: list[dict], reason: str) -> None:
    with pytest.raises(TranscriptError, match=reason):
        transcript_of(segments, act=acts)
