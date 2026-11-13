"""The part of the video that a command names."""

import pytest

from tests.factories import segment_table, splash_table, transcript_of
from tools.project import part_of


def test_a_command_gets_the_transcript_of_the_part_that_it_names() -> None:
    transcript = transcript_of([segment_table("opening"), segment_table("node")], splash=splash_table())

    assert part_of(transcript, "node") == transcript.part("node")
    assert part_of(transcript, "splash") == transcript.part("splash")


def test_a_command_stops_on_a_part_that_the_video_does_not_have() -> None:
    transcript = transcript_of([segment_table("opening"), segment_table("node")], splash=splash_table())

    with pytest.raises(SystemExit, match="The video has no part `nod`. Its parts are: splash, opening, node."):
        part_of(transcript, "nod")
