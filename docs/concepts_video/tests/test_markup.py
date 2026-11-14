"""The marks of the texts of the transcript."""

from tools import markup


def test_the_technical_names_of_a_text_are_its_spans_between_backticks() -> None:
    assert markup.code_spans("Run `peppy node sync` in the node, then `uv run <node>`.") == [
        "peppy node sync",
        "uv run <node>",
    ]


def test_a_viewer_reads_the_text_with_no_backtick() -> None:
    assert markup.without_marks("One file, `peppy.json5`, declares the node.") == "One file, peppy.json5, declares the node."


def test_the_parts_keep_the_order_of_the_text() -> None:
    assert markup.parts("Add `--with <option>` now") == [(False, "Add "), (True, "--with <option>"), (False, " now")]


def test_the_voice_says_the_longest_written_word_first() -> None:
    pronunciation = {"peppy": "peh pee", "peppy.json5": "peppy dot jason five"}

    assert markup.spoken("`peppy.json5` is the file of peppy.", pronunciation) == (
        "peppy dot jason five is the file of peh pee."
    )


def test_the_voice_changes_full_words_only() -> None:
    assert markup.spoken("The `uv` tool and the uvicorn server.", {"uv": "you vee"}) == (
        "The you vee tool and the uvicorn server."
    )


def test_a_text_splits_into_its_sentences() -> None:
    assert markup.sentences("A node runs alone. It tests alone! Does it?") == [
        "A node runs alone.",
        "It tests alone!",
        "Does it?",
    ]


def test_a_command_counts_one_word_for_each_of_its_words() -> None:
    assert markup.word_count(markup.without_marks("The command `peppy node sync` reads the file.")) == 8
