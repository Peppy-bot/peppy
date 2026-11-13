"""The checks of the transcript against the docs."""

from pathlib import Path

import pytest

from tests.factories import segment_table, transcript_of
from tools import checks
from tools.docs_sections import read_section
from tools.transcript import CodeBlock

PAGE = """---
title: A page
---

## A section

Run `peppy stack launch openarm_simulation --with mujoco`, then `uv run --group dev pytest`.

```python
standalone_config = StandaloneConfig().with_parameters(params)
```

The node file is `peppy.json5`, with `build_cmd` and `run_cmd`.
"""


@pytest.fixture
def docs_root(tmp_path: Path) -> Path:
    (tmp_path / "guides").mkdir()
    (tmp_path / "guides" / "page.mdx").write_text(PAGE, encoding="utf-8")
    return tmp_path


def source_of(docs_root: Path) -> dict:
    sha256 = read_section(docs_root, "guides/page.mdx", "A section").sha256
    return {"page": "guides/page.mdx", "section": "A section", "sha256": sha256}


@pytest.mark.parametrize(
    ("span", "fragments"),
    [
        ("peppy.json5", ["peppy.json5"]),
        ("peppy stack launch <launcher> --with <option>", ["peppy stack launch", "--with"]),
        ("peppy stack launch --place <link>@<core node> <launcher>", ["peppy stack launch", "--place"]),
        ("uv run --group dev pytest", ["uv run", "--group", "dev pytest"]),
        ("curl -fsSL https://peppy.bot/install.sh | sh", ["curl", "-fsSL", "https://peppy.bot/install.sh", "sh"]),
        ("source /opt/ros/<distro>/setup.bash", ["source /opt/ros/", "/setup.bash"]),
    ],
)
def test_the_fragments_of_a_technical_name(span: str, fragments: list[str]) -> None:
    assert checks.fragments_of_span(span) == fragments


@pytest.mark.parametrize(
    ("text", "fragment", "holds"),
    [
        ("run `peppy stack launch` now", "peppy stack launch", True),
        ("run peppy  stack\n launch now", "peppy stack launch", True),
        ("run peppy stack list now", "peppy stack launch", False),
        ("the `ament_cmake` package", "ament", False),
        ("the `ament` build system", "ament", True),
        ("a built-in server", "-i", False),
        ("rosdep install -i --from-path src", "-i", True),
        ("pass --with=zed to it", "--with", True),
        ("pass --without to it", "--with", False),
    ],
)
def test_a_text_holds_a_fragment_as_full_words_only(text: str, fragment: str, holds: bool) -> None:
    assert checks.holds_fragment(text, fragment) is holds


def test_a_python_block_gives_its_names_but_the_words_of_python_and_of_its_example() -> None:
    block = CodeBlock(
        language="python",
        code='async with harness.start(setup) as h:\n    await h.mocks.deps.camera.publish(Message(width=640, frame=b"x"))',
        example_names=("camera", "width", "frame"),
    )

    assert checks.fragments_of_code_block(block) == ["harness", "start", "setup", "h", "mocks", "deps", "publish", "Message"]


def test_a_json5_block_gives_its_names_outside_of_its_strings() -> None:
    block = CodeBlock(
        language="json5",
        code='{\n  manifest: { name: "camera" }, // a comment\n  run_cmd: ["uv", "run"], on: true,\n}',
        example_names=("on",),
    )

    assert checks.fragments_of_code_block(block) == ["manifest", "name", "run_cmd"]


def test_a_shell_block_gives_the_fragments_of_each_command() -> None:
    block = CodeBlock(language="sh", code="peppy repo add <repository>\npeppy repo refresh", example_names=())

    assert checks.fragments_of_code_block(block) == ["peppy repo add", "peppy repo refresh"]


def test_a_transcript_that_agrees_with_its_sections_has_no_problem(docs_root: Path) -> None:
    transcript = transcript_of(
        [
            segment_table(
                sentences=[{"id": "launch", "text": "Run `peppy stack launch <launcher> --with <option>`."}],
                source=[source_of(docs_root)],
                screen={"file": "`peppy.json5`", "code": {"language": "python", "code": "StandaloneConfig().with_parameters(params)"}},
            )
        ]
    )

    assert checks.source_problems(transcript, docs_root) == []
    assert checks.technical_name_problems(transcript, docs_root) == []


def test_a_technical_name_that_the_sections_do_not_hold_is_a_problem(docs_root: Path) -> None:
    transcript = transcript_of(
        [
            segment_table(
                sentences=[{"id": "launch", "text": "Run `peppy stack start <launcher> --fast`."}],
                source=[source_of(docs_root)],
                screen={"code": {"language": "python", "code": "NodeBuilder().spin(setup)", "example_names": ["setup"]}},
            )
        ]
    )

    assert checks.technical_name_problems(transcript, docs_root) == [
        "Segment `opening`, sentence `launch`: no section that the segment names holds `peppy stack start`.",
        "Segment `opening`, sentence `launch`: no section that the segment names holds `--fast`.",
        "Segment `opening`, screen.code: no section that the segment names holds `NodeBuilder`.",
        "Segment `opening`, screen.code: no section that the segment names holds `spin`.",
    ]


def test_a_section_whose_text_changed_is_a_problem_that_gives_the_new_hash(docs_root: Path) -> None:
    transcript = transcript_of([segment_table(source=[{**source_of(docs_root), "sha256": "1" * 64}])])

    (problem,) = checks.source_problems(transcript, docs_root)

    assert "the text of the section `A section` of guides/page.mdx changed" in problem
    assert source_of(docs_root)["sha256"] in problem


def test_a_source_that_names_no_section_is_a_problem(docs_root: Path) -> None:
    source = {"page": "guides/page.mdx", "section": "No such section", "sha256": "1" * 64}

    assert checks.source_problems(transcript_of([segment_table(source=[source])]), docs_root) == [
        "Segment `opening`: guides/page.mdx: no section has the heading `No such section`."
    ]


def test_a_sentence_of_the_voice_with_more_than_25_words_is_a_problem() -> None:
    long_sentence = " ".join(["word"] * 26) + "."
    at_the_limit = " ".join(["word"] * 25) + "."
    transcript = transcript_of(
        [segment_table(sentences=[{"id": "long", "text": f"Short. {long_sentence}"}, {"id": "limit", "text": at_the_limit}])]
    )

    (problem,) = checks.sentence_length_problems(transcript)

    assert problem.startswith("Segment `opening`, sentence `long`: 26 words, and the limit is 25")
