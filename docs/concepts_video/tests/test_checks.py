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


def docs_root_copy(docs_root: Path, destination: Path) -> Path:
    """A copy of the docs pages, as the baseline of a check."""
    (destination / "guides").mkdir(parents=True)
    (destination / "guides" / "page.mdx").write_text((docs_root / "guides" / "page.mdx").read_text(encoding="utf-8"), encoding="utf-8")
    return destination


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


def test_a_transcript_that_agrees_with_its_sections_has_no_finding(docs_root: Path) -> None:
    transcript = transcript_of(
        [
            segment_table(
                sentences=[{"id": "launch", "text": "Run `peppy stack launch <launcher> --with <option>`."}],
                source=[source_of(docs_root)],
                screen={"file": "`peppy.json5`", "code": {"language": "python", "code": "StandaloneConfig().with_parameters(params)"}},
            )
        ]
    )

    assert checks.docs_findings(transcript, docs_root) == []


def test_a_technical_name_that_the_sections_do_not_hold_is_a_finding(docs_root: Path) -> None:
    transcript = transcript_of(
        [
            segment_table(
                sentences=[{"id": "launch", "text": "Run `peppy stack start <launcher> --fast`."}],
                source=[source_of(docs_root)],
                screen={"code": {"language": "python", "code": "NodeBuilder().spin(setup)", "example_names": ["setup"]}},
            )
        ]
    )

    (found,) = checks.docs_findings(transcript, docs_root)

    assert found.segment_id == "opening"
    assert found.changed_sections == ()
    assert found.missing_names == (
        checks.MissingName(where="sentence `launch`", name="peppy stack start"),
        checks.MissingName(where="sentence `launch`", name="--fast"),
        checks.MissingName(where="screen.code", name="NodeBuilder"),
        checks.MissingName(where="screen.code", name="spin"),
    )
    assert checks.problems_of([found]) == [
        "Segment `opening`, sentence `launch`: no section that the segment names holds `peppy stack start`.",
        "Segment `opening`, sentence `launch`: no section that the segment names holds `--fast`.",
        "Segment `opening`, screen.code: no section that the segment names holds `NodeBuilder`.",
        "Segment `opening`, screen.code: no section that the segment names holds `spin`.",
    ]


def test_a_section_whose_text_is_not_the_recorded_text_is_a_changed_section(docs_root: Path) -> None:
    transcript = transcript_of([segment_table(source=[{**source_of(docs_root), "sha256": "1" * 64}])])

    (found,) = checks.docs_findings(transcript, docs_root)

    assert found.changed_sections == (checks.ChangedSection(page="guides/page.mdx", section="A section", diff=None),)
    assert checks.problems_of([found]) == [
        "Segment `opening`: the text of the section `A section` of guides/page.mdx changed. "
        "Read the segment against the section, correct the transcript if necessary, "
        "then run `pixi run record-sections`."
    ]


def test_a_section_that_has_the_text_of_the_baseline_is_not_a_changed_section(docs_root: Path, tmp_path: Path) -> None:
    baseline_root = docs_root_copy(docs_root, tmp_path / "baseline")
    transcript = transcript_of([segment_table(source=[{**source_of(docs_root), "sha256": "1" * 64}])])

    assert checks.docs_findings(transcript, docs_root, baseline_root) == []


def test_a_section_that_changed_since_the_baseline_gives_the_lines_that_changed(docs_root: Path, tmp_path: Path) -> None:
    baseline_root = docs_root_copy(docs_root, tmp_path / "baseline")
    recorded = source_of(docs_root)
    page = docs_root / "guides" / "page.mdx"
    page.write_text(page.read_text(encoding="utf-8").replace("with `build_cmd` and `run_cmd`", "with `run_cmd`"), encoding="utf-8")

    (found,) = checks.docs_findings(transcript_of([segment_table(source=[recorded])]), docs_root, baseline_root)

    (changed,) = found.changed_sections
    assert "-The node file is `peppy.json5`, with `build_cmd` and `run_cmd`." in changed.diff
    assert "+The node file is `peppy.json5`, with `run_cmd`." in changed.diff


def test_a_section_that_the_transcript_records_is_not_a_changed_section_with_another_baseline(
    docs_root: Path, tmp_path: Path
) -> None:
    baseline_root = docs_root_copy(docs_root, tmp_path / "baseline")
    page = docs_root / "guides" / "page.mdx"
    page.write_text(page.read_text(encoding="utf-8").replace("with `build_cmd` and `run_cmd`", "with `run_cmd`"), encoding="utf-8")

    assert checks.docs_findings(transcript_of([segment_table(source=[source_of(docs_root)])]), docs_root, baseline_root) == []


def test_a_section_that_the_baseline_does_not_have_is_a_changed_section_with_no_diff(docs_root: Path, tmp_path: Path) -> None:
    baseline_root = tmp_path / "baseline"
    baseline_root.mkdir()
    transcript = transcript_of([segment_table(source=[{**source_of(docs_root), "sha256": "1" * 64}])])

    (found,) = checks.docs_findings(transcript, docs_root, baseline_root)

    assert found.changed_sections == (checks.ChangedSection(page="guides/page.mdx", section="A section", diff=None),)


def test_a_source_that_names_no_section_is_a_finding(docs_root: Path) -> None:
    source = {"page": "guides/page.mdx", "section": "No such section", "sha256": "1" * 64}

    (found,) = checks.docs_findings(transcript_of([segment_table(source=[source])]), docs_root)

    assert found.missing_sections == ("guides/page.mdx: no section has the heading `No such section`",)
    assert checks.problems_of([found]) == [
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
