"""Compiles the compositions of the video into a HyperFrames project.

`compositions/` holds the composition of each segment, the composition of the
splash screen and the root composition, as Jinja templates. A composition
holds the layout and the animation, and no text of its own: each text comes
from the transcript as `screen.<key>`, and each time comes from the timeline as
`at.<sentence id>.start` and `at.<sentence id>.end`, in seconds from the
start of the segment. The compiled project has these values as plain HTML,
which HyperFrames checks and renders.
"""

from pathlib import Path
import re
import shutil
import subprocess
from typing import Iterator, Mapping

import jinja2
from markupsafe import Markup, escape

from tools import markup, mix, music, paths
from tools.footage import FootageFile
from tools.timing import FRAMES_PER_SECOND, SegmentSlot, Timeline
from tools.transcript import SPLASH_ID, CodeBlock, ScreenText, Segment, Splash, Transcript, screen_leaves

ROOT_TEMPLATE = "index.html.j2"
TEMPLATE_SUFFIX = ".html.j2"
# The voice and the music of the video, in the compiled project. The
# repository does not hold them: the build makes them.
VOICE_TRACK = "assets/voice.wav"
MUSIC_TRACK = "assets/music.wav"
# The templates that a composition imports. They are not compositions.
SHARED_PREFIX = "_"
# The animation functions that each composition uses.
MOTION_SCRIPT = paths.COMPOSITIONS / "motion.js"
# The 3D view of the robot, which esbuild bundles with three.js.
ROBOT_VIEW_MODULE = paths.COMPOSITIONS / "robot_view.js"
ESBUILD = paths.NODE_MODULES / ".bin" / "esbuild"
GSAP = paths.NODE_MODULES / "gsap" / "dist" / "gsap.min.js"

# The kinds of token that a code window gives a colour, for each language.
STRING_TOKEN = r"""(?P<string>b?"(?:\\.|[^"\\])*"|b?'(?:\\.|[^'\\])*')"""
NUMBER_TOKEN = r"(?P<number>\b\d+(?:\.\d+)?\b)"
CODE_TOKENS = {
    "python": re.compile(
        STRING_TOKEN
        + r"|(?P<keyword>\b(?:async|def|with|as|await|assert|return|import|from|if|else|for|in|not)\b)|"
        + NUMBER_TOKEN
    ),
    "json5": re.compile(STRING_TOKEN + r"|(?P<key>\b[A-Za-z_]\w*(?=\s*:))|" + NUMBER_TOKEN),
}

JINJA_TAG = re.compile(r"\{\{.*?\}\}|\{%.*?%\}|\{#.*?#\}", re.DOTALL)
SCRIPT_OR_STYLE = re.compile(r"<(script|style)\b.*?</\1>", re.DOTALL | re.IGNORECASE)
HTML_TAG = re.compile(r"<[^<>]*>")
HTML_ENTITY = re.compile(r"&#?\w+;")


class CompileError(Exception):
    """A composition and the transcript do not agree."""


class TrackedTable:
    """A table of the screen texts of a segment, which records each text that a composition reads."""

    def __init__(self, table: Mapping[str, ScreenText], path: str, read_paths: set[str]) -> None:
        self._table = table
        self._path = path
        self._read_paths = read_paths

    def __getattr__(self, key: str) -> object:
        return self[key]

    def __getitem__(self, key: str) -> object:
        if key not in self._table:
            raise CompileError(f"the transcript has no text `{self._path}.{key}`")
        return _tracked(self._table[key], f"{self._path}.{key}", self._read_paths)

    def __contains__(self, key: str) -> bool:
        return key in self._table

    def items(self) -> Iterator[tuple[str, object]]:
        for key in self._table:
            yield key, self[key]


def _tracked(value: ScreenText, path: str, read_paths: set[str]) -> object:
    if isinstance(value, (str, CodeBlock)):
        read_paths.add(path)
        return value
    if isinstance(value, tuple):
        return [_tracked(item, f"{path}[{index}]", read_paths) for index, item in enumerate(value)]
    return TrackedTable(value, path, read_paths)


def compile_project(
    transcript: Transcript, timeline: Timeline, footage: tuple[FootageFile, ...], destination: Path
) -> None:
    """Write the HyperFrames project of the video into `destination`, which it replaces."""
    if destination.exists():
        shutil.rmtree(destination)
    (destination / "compositions").mkdir(parents=True)
    environment = _environment(transcript, footage)
    for segment, slot in zip(transcript.segments, timeline.segments, strict=True):
        composition = _render_composition(environment, segment, slot)
        (destination / "compositions" / f"{segment.id}.html").write_text(composition, encoding="utf-8")
    if transcript.splash is not None:
        splash = _render_splash(environment, transcript.splash, timeline.splash_duration)
        (destination / "compositions" / f"{SPLASH_ID}.html").write_text(splash, encoding="utf-8")
    root = environment.get_template(ROOT_TEMPLATE).render(
        splash_id=SPLASH_ID,
        splash_duration=timeline.splash_duration,
        segments=timeline.segments,
        duration=timeline.duration,
        frames_per_second=FRAMES_PER_SECOND,
        voice_track=VOICE_TRACK,
        music_track=MUSIC_TRACK,
        music_automation=mix.volume_automation(mix.music_levels(timeline)),
    )
    (destination / "index.html").write_text(root, encoding="utf-8")
    shutil.copytree(paths.ASSETS, destination / "assets")
    shutil.copytree(paths.STYLES, destination / "styles")
    (destination / "lib").mkdir()
    shutil.copyfile(GSAP, destination / "lib" / GSAP.name)
    shutil.copyfile(MOTION_SCRIPT, destination / "lib" / MOTION_SCRIPT.name)
    _bundle_robot_view(destination / "lib" / ROBOT_VIEW_MODULE.name)


def link_robot_model(model: Path, destination: Path) -> None:
    """Put the 3D model of the robot into the compiled project."""
    folder = destination / "assets" / "models"
    folder.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(model, folder / model.name)


def link_footage(footage: tuple[FootageFile, ...], destination: Path) -> None:
    """Put each footage file of the cache into the compiled project."""
    folder = destination / "assets" / "footage"
    folder.mkdir(parents=True, exist_ok=True)
    for file in footage:
        shutil.copyfile(file.cache_path, folder / file.name)


def composition_templates() -> list[Path]:
    """The template of each composition: of each segment, of the splash screen and of the root."""
    return sorted(
        path for path in paths.COMPOSITIONS.glob(f"*{TEMPLATE_SUFFIX}") if not path.name.startswith(SHARED_PREFIX)
    )


def literal_text_of_template(source: str) -> str:
    """The text that a template shows by itself, with no value of the transcript."""
    without_code = SCRIPT_OR_STYLE.sub(" ", JINJA_TAG.sub(" ", source))
    return HTML_ENTITY.sub(" ", HTML_TAG.sub(" ", without_code))


def _bundle_robot_view(destination: Path) -> None:
    """Write the robot view and three.js as one script that gives `RobotView`."""
    subprocess.run(
        [
            str(ESBUILD),
            str(ROBOT_VIEW_MODULE),
            "--bundle",
            "--format=iife",
            "--global-name=RobotView",
            "--log-level=warning",
            f"--outfile={destination}",
        ],
        cwd=paths.PROJECT_ROOT,
        check=True,
    )


def _environment(transcript: Transcript, footage: tuple[FootageFile, ...]) -> jinja2.Environment:
    environment = jinja2.Environment(
        loader=jinja2.FileSystemLoader(paths.COMPOSITIONS),
        autoescape=True,
        undefined=jinja2.StrictUndefined,
        trim_blocks=True,
        lstrip_blocks=True,
    )
    environment.filters["rich"] = _rich
    environment.filters["plain"] = markup.without_marks
    environment.filters["code"] = highlighted_code_line
    environment.globals["labels"] = transcript.labels
    environment.globals["footage"] = _footage_path_function(footage)
    return environment


def _render_composition(environment: jinja2.Environment, segment: Segment, slot: SegmentSlot) -> str:
    name = f"{segment.id}{TEMPLATE_SUFFIX}"
    try:
        template = environment.get_template(name)
    except jinja2.TemplateNotFound:
        raise CompileError(f"the segment `{segment.id}` has no composition compositions/{name}") from None
    return _render_with_each_text(
        template,
        segment.screen,
        f"the segment `{segment.id}`",
        segment=segment,
        number=segment.number,
        start=slot.start,
        duration=slot.duration,
        at={sentence.id: {"start": sentence.start, "end": sentence.end} for sentence in slot.sentences},
    )


def _render_splash(environment: jinja2.Environment, splash: Splash, duration: float) -> str:
    return _render_with_each_text(
        environment.get_template(f"{SPLASH_ID}{TEMPLATE_SUFFIX}"),
        splash.screen,
        "the splash screen",
        id=SPLASH_ID,
        duration=duration,
        bells=music.opening_bell_seconds(),
    )


def _render_with_each_text(
    template: jinja2.Template, screen: Mapping[str, ScreenText], owner: str, **values: object
) -> str:
    """Render a composition with the texts of `screen`. Refuse a composition that does not show each text."""
    read_paths: set[str] = set()
    rendered = template.render(screen=TrackedTable(screen, "screen", read_paths), **values)
    not_shown = [path for path, _ in screen_leaves(screen) if path not in read_paths]
    if not_shown:
        raise CompileError(f"the composition of {owner} does not show {', '.join(not_shown)}")
    return rendered


def _rich(text: str) -> Markup:
    """Prose as HTML, with each technical name in a `<code>` element."""
    html = [
        Markup("<code>{}</code>").format(part) if is_technical_name else escape(part)
        for is_technical_name, part in markup.parts(text)
    ]
    return Markup("").join(html)


def highlighted_code_line(line: str, language: str) -> Markup:
    """One line of code as HTML, with a colour class for each kind of token."""
    tokens = CODE_TOKENS.get(language)
    if tokens is None:
        return escape(line)
    html: list[Markup] = []
    position = 0
    for token in tokens.finditer(line):
        html.append(escape(line[position : token.start()]))
        html.append(Markup('<span class="code-{}">{}</span>').format(token.lastgroup, token.group()))
        position = token.end()
    html.append(escape(line[position:]))
    return Markup("").join(html)


def _footage_path_function(footage: tuple[FootageFile, ...]):
    listed = {file.name for file in footage}

    def footage_path(name: str) -> str:
        if name not in listed:
            raise CompileError(f"a composition names the footage file `{name}`, which is not in footage.json")
        return f"assets/footage/{name}"

    return footage_path
