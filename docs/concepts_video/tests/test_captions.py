"""The captions and the chapters of the video."""

from tests.factories import segment_table, transcript_of
from tools.captions import cues_of_sentence, split_evenly, subrip_captions, youtube_chapters
from tools.timing import lay_out


def test_a_short_sentence_is_one_cue_of_one_line() -> None:
    assert cues_of_sentence("A node has one function.", 1.0, 3.0) == [(1.0, 3.0, "A node has one function.")]


def test_a_sentence_of_more_than_one_line_has_two_lines_of_about_the_same_length() -> None:
    ((_, _, text),) = cues_of_sentence("A driver reads each camera, and a driver moves each motor.", 0.0, 4.0)

    assert text == "A driver reads each camera,\nand a driver moves each motor."


def test_a_long_sentence_has_cues_that_share_its_time_and_fit_two_lines_each() -> None:
    sentence = (
        "It is then a regular program: you start it in your IDE with a debugger, "
        "and it communicates with the nodes of the stack."
    )

    cues = cues_of_sentence(sentence, 10.0, 16.0)

    assert len(cues) == 2
    assert cues[0][0] == 10.0 and cues[-1][1] == 16.0
    assert cues[0][1] == cues[1][0]
    assert " ".join(text.replace("\n", " ") for _, _, text in cues) == sentence
    assert all(len(line) <= 44 for _, _, text in cues for line in text.split("\n"))


def test_an_even_split_keeps_each_word_whole() -> None:
    assert split_evenly("one two three four", 2) == ["one two", "three four"]
    assert split_evenly("word", 3) == ["word"]


def test_the_captions_show_each_sentence_while_the_voice_says_it() -> None:
    transcript = transcript_of(
        [
            segment_table("opening", [{"id": "only", "text": "The file is `peppy.json5`."}]),
            segment_table("node", [{"id": "only", "text": "A node has one function."}]),
        ]
    )
    timeline = lay_out(transcript, {"opening/only": 2.0, "node/only": 1.5})

    assert subrip_captions(transcript, timeline) == (
        "1\n00:00:01,000 --> 00:00:03,000\nThe file is peppy.json5.\n"
        "\n"
        "2\n00:00:06,000 --> 00:00:07,500\nA node has one function.\n"
    )


def test_the_chapters_give_the_start_and_the_title_of_each_segment() -> None:
    transcript = transcript_of(
        [
            segment_table("opening", chapter="Why a robot needs a framework"),
            segment_table("node", chapter="A node and its file"),
        ]
    )
    timeline = lay_out(transcript, {"opening/only": 62.0, "node/only": 1.5})

    assert youtube_chapters(transcript, timeline) == "0:00 Why a robot needs a framework\n1:05 A node and its file\n"
