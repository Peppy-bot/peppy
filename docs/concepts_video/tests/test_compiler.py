"""The compile step of the compositions."""

import pytest

from tools import compiler
from tools.compiler import CompileError, TrackedTable
from tools.footage import FootageFile


def test_the_literal_text_of_a_template_has_no_value_no_script_and_no_style() -> None:
    template = """{# A comment #}
<style>#root { color: red; }</style>
<div class="label">{{ screen.title }}</div>
{% for step in screen.steps %}<li>{{ step | rich }}</li>{% endfor %}
<script>const words = "not on the screen";</script>
<span>&nbsp;01</span>"""

    assert compiler.literal_text_of_template(template).split() == ["01"]


def test_a_tracked_table_records_each_text_that_a_composition_reads() -> None:
    read_paths: set[str] = set()
    screen = TrackedTable({"title": "A title", "cards": {"node": {"term": "Node"}}, "steps": ("One", "Two")}, "screen", read_paths)

    assert screen.cards.node.term == "Node"
    assert screen["steps"] == ["One", "Two"]

    assert read_paths == {"screen.cards.node.term", "screen.steps[0]", "screen.steps[1]"}
    assert "title" in screen and "absent" not in screen


def test_a_tracked_table_refuses_a_text_that_the_transcript_does_not_have() -> None:
    screen = TrackedTable({"cards": {}}, "screen", set())

    with pytest.raises(CompileError, match="the transcript has no text `screen.cards.node`"):
        screen.cards.node


@pytest.mark.parametrize(
    ("line", "language", "html"),
    [
        ("async with harness.start(setup) as h:", "python", '<span class="code-keyword">async</span> <span class="code-keyword">with</span> harness.start(setup) <span class="code-keyword">as</span> h:'),
        ('name: "camera", size: 640,', "json5", '<span class="code-key">name</span>: <span class="code-string">&#34;camera&#34;</span>, <span class="code-key">size</span>: <span class="code-number">640</span>,'),
        ("peppy stack launch <launcher>", "sh", "peppy stack launch &lt;launcher&gt;"),
    ],
)
def test_a_code_line_has_a_colour_class_for_each_kind_of_token(line: str, language: str, html: str) -> None:
    assert str(compiler.highlighted_code_line(line, language)) == html


def test_a_composition_names_only_the_footage_of_the_list() -> None:
    listed = (FootageFile(name="opening_two_arms.mp4", sha256="a" * 64, key="a" * 64 + "/opening_two_arms.mp4"),)
    footage_path = compiler._footage_path_function(listed)

    assert footage_path("opening_two_arms.mp4") == "assets/footage/opening_two_arms.mp4"
    with pytest.raises(CompileError, match="`absent.mp4`, which is not in footage.json"):
        footage_path("absent.mp4")
