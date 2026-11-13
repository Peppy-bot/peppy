"""The compile step of the compositions."""

from dataclasses import replace
import html
import json
from pathlib import Path
import re

import pytest

from tools import compiler, mix, paths
from tools.compiler import CompileError, TrackedTable
from tools.footage import FootageFile
from tools.project import timeline_of
from tools.timing import Timeline
from tools.transcript import SPLASH_ID, Transcript, read_transcript
from tools.voice_durations import read_voice_durations


@pytest.fixture(scope="module")
def transcript() -> Transcript:
    return read_transcript(paths.TRANSCRIPT)


def timeline(transcript: Transcript) -> Timeline:
    return timeline_of(transcript, read_voice_durations(paths.VOICE_DURATIONS))


def compiled_root(transcript: Transcript, destination: Path) -> str:
    """Compile the video of `transcript` with the compositions of the project, and give its root composition."""
    compiler.compile_project(transcript, timeline(transcript), (), destination)
    return (destination / "index.html").read_text(encoding="utf-8")


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


def test_a_video_with_a_splash_screen_shows_it_before_the_first_segment(transcript: Transcript, tmp_path: Path) -> None:
    root = compiled_root(transcript, tmp_path / "project")

    splash = (tmp_path / "project" / "compositions" / f"{SPLASH_ID}.html").read_text(encoding="utf-8")
    assert all(text in splash for text in transcript.splash.screen.values())
    assert f'data-composition-src="compositions/{SPLASH_ID}.html"' in root
    assert f'data-duration="{timeline(transcript).splash_duration}"' in root


def test_the_video_of_one_segment_has_no_splash_screen(transcript: Transcript, tmp_path: Path) -> None:
    root = compiled_root(transcript.part(transcript.segments[0].id), tmp_path / "project")

    assert SPLASH_ID not in root
    assert not (tmp_path / "project" / "compositions" / f"{SPLASH_ID}.html").exists()


def test_a_text_of_the_splash_screen_that_its_composition_does_not_show_is_refused(
    transcript: Transcript, tmp_path: Path
) -> None:
    splash = replace(transcript.splash, screen={**transcript.splash.screen, "extra": "Not on the screen"})

    with pytest.raises(CompileError, match="the composition of the splash screen does not show screen.extra"):
        compiled_root(replace(transcript, splash=splash), tmp_path / "project")


def test_the_root_composition_plays_the_voice_and_the_music_with_its_levels(
    transcript: Transcript, tmp_path: Path
) -> None:
    root = compiled_root(transcript, tmp_path / "project")

    automation = json.loads(html.unescape(re.search(r'data-automation="([^"]*)"', root).group(1)))
    points = [(point["t"], point["v"]) for point in automation["lanes"][0]["points"]]
    assert points == mix.music_levels(timeline(transcript))
    assert f'src="{compiler.VOICE_TRACK}"' in root and f'src="{compiler.MUSIC_TRACK}"' in root
