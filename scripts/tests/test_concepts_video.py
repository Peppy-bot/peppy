"""Tests for functions.concepts_video."""

from __future__ import annotations

import json
import tomllib
from collections.abc import Iterator
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from functions.cli import ReleaseError
from functions.concepts_video import (
    ChangedSection,
    MissingName,
    SegmentFindings,
    SegmentOutcome,
    SegmentStatus,
    VideoCheck,
    VideoUpdate,
    check_video,
    record_sections,
    update_video,
)

REPO_ROOT = Path("/repo")
VIDEO_DIR = REPO_ROOT / "docs/concepts_video"
CONTENT_SHA256 = "c" * 64

_CHANGED_SECTION = SegmentFindings(
    segment_id="launchers",
    changed_sections=(
        ChangedSection(
            page="quickstart.mdx",
            section="Quickstart",
            diff="--- last release\n+++ now\n-Run `--with mujoco`.\n+Run `--sim mujoco`.",
        ),
        ChangedSection(page="guides/new_page.mdx", section="A new section", diff=None),
    ),
    missing_sections=(),
    missing_names=(),
)
_DISAGREEING = SegmentFindings(
    segment_id="stack",
    changed_sections=(),
    missing_sections=("guides/node_stack.mdx: no section has the heading `Gone`",),
    missing_names=(MissingName(where="sentence `launch`", name="--with"),),
)


def _report(*segments: dict, upload_is_current: bool = True) -> dict:
    """The document `docs-check --json` prints."""
    return {
        "segments": list(segments),
        "content_sha256": CONTENT_SHA256,
        "upload_is_current": upload_is_current,
    }


def _completed(stdout: str = "", returncode: int = 0, stderr: str = "") -> MagicMock:
    return MagicMock(returncode=returncode, stdout=stdout, stderr=stderr)


# --- the docs check of the video project ---


@patch("functions.concepts_video.subprocess.run")
def test_check_video_runs_the_docs_check_of_the_video_project_against_the_baseline(
    mock_run: MagicMock,
) -> None:
    mock_run.return_value = _completed(json.dumps(_report()))

    check = check_video(REPO_ROOT, Path("/last-release/docs/src/content/docs"))

    assert check == VideoCheck(
        segments=(), content_sha256=CONTENT_SHA256, upload_is_current=True
    )
    # The task runs in the pixi workspace of the video project, with its
    # lockfile as it is.
    assert mock_run.call_args.args[0] == [
        "pixi",
        "run",
        "--locked",
        "--manifest-path",
        str(VIDEO_DIR / "pixi.toml"),
        "docs-check",
        "--json",
        "--baseline",
        "/last-release/docs/src/content/docs",
    ]
    assert mock_run.call_args.kwargs["cwd"] == VIDEO_DIR


@patch("functions.concepts_video.subprocess.run")
def test_check_video_without_a_baseline_names_none(mock_run: MagicMock) -> None:
    mock_run.return_value = _completed(json.dumps(_report()))

    check_video(REPO_ROOT, None)

    assert mock_run.call_args.args[0][-2:] == ["docs-check", "--json"]


@patch("functions.concepts_video.subprocess.run")
def test_check_video_reads_the_findings_of_each_segment(mock_run: MagicMock) -> None:
    mock_run.return_value = _completed(
        json.dumps(
            _report(
                {
                    "segment_id": "launchers",
                    "changed_sections": [
                        {
                            "page": "quickstart.mdx",
                            "section": "Quickstart",
                            "diff": "-old\n+new",
                        },
                        {
                            "page": "guides/new_page.mdx",
                            "section": "A new section",
                            "diff": None,
                        },
                    ],
                    "missing_sections": [],
                    "missing_names": [],
                },
                {
                    "segment_id": "stack",
                    "changed_sections": [],
                    "missing_sections": [
                        "guides/node_stack.mdx: no section has the heading `Gone`"
                    ],
                    "missing_names": [{"where": "sentence `launch`", "name": "--with"}],
                },
                upload_is_current=False,
            )
        )
    )

    check = check_video(REPO_ROOT, None)

    assert check.upload_is_current is False
    assert check.segments == (
        SegmentFindings(
            segment_id="launchers",
            changed_sections=(
                ChangedSection(
                    page="quickstart.mdx", section="Quickstart", diff="-old\n+new"
                ),
                ChangedSection(
                    page="guides/new_page.mdx", section="A new section", diff=None
                ),
            ),
            missing_sections=(),
            missing_names=(),
        ),
        _DISAGREEING,
    )


def test_a_changed_section_alone_is_not_a_disagreement() -> None:
    # A section that changed can leave its segment correct. A section that is
    # gone, or a name the docs no longer hold, cannot.
    assert _CHANGED_SECTION.disagreements == ()
    assert _DISAGREEING.disagreements == (
        "Segment `stack`: guides/node_stack.mdx: no section has the heading `Gone`.",
        "Segment `stack`, sentence `launch`: no section that the segment names "
        "holds `--with`.",
    )
    check = VideoCheck(
        segments=(_CHANGED_SECTION, _DISAGREEING),
        content_sha256=CONTENT_SHA256,
        upload_is_current=True,
    )
    assert check.disagreements == _DISAGREEING.disagreements


@patch("functions.concepts_video.subprocess.run")
def test_check_video_reports_a_task_that_fails_with_what_it_printed(
    mock_run: MagicMock,
) -> None:
    mock_run.return_value = _completed(
        returncode=1, stderr="segment `opening`: `chapter` is not a text"
    )

    with pytest.raises(ReleaseError) as excinfo:
        check_video(REPO_ROOT, None)

    message = str(excinfo.value)
    assert "the docs check of the concepts video failed (exit 1)" in message
    assert "`chapter` is not a text" in message


@patch("functions.concepts_video.subprocess.run")
def test_check_video_refuses_output_that_is_not_json(mock_run: MagicMock) -> None:
    mock_run.return_value = _completed("The transcript agrees with the docs.")

    with pytest.raises(ReleaseError, match="did not print valid JSON"):
        check_video(REPO_ROOT, None)


@pytest.mark.parametrize(
    ("report", "problem"),
    [
        ([], "is not an object"),
        ({"content_sha256": CONTENT_SHA256, "upload_is_current": True}, "'segments'"),
        ({"segments": [], "upload_is_current": True}, "'content_sha256'"),
        (
            {"segments": [], "content_sha256": CONTENT_SHA256, "upload_is_current": 1},
            "'upload_is_current'",
        ),
        (_report("launchers"), "segment must be an object"),
        (_report({"segment_id": "launchers"}), "has a bad list"),
        (
            _report(
                {
                    "segment_id": "launchers",
                    "changed_sections": [{"page": "quickstart.mdx"}],
                    "missing_sections": [],
                    "missing_names": [],
                }
            ),
            "missing page/section",
        ),
        (
            _report(
                {
                    "segment_id": "launchers",
                    "changed_sections": [],
                    "missing_sections": [],
                    "missing_names": [{"where": "sentence `launch`"}],
                }
            ),
            "missing where/name",
        ),
    ],
)
@patch("functions.concepts_video.subprocess.run")
def test_check_video_refuses_a_report_in_another_shape(
    mock_run: MagicMock, report: object, problem: str
) -> None:
    mock_run.return_value = _completed(json.dumps(report))

    with pytest.raises(ReleaseError, match=problem):
        check_video(REPO_ROOT, None)


@patch("functions.concepts_video.subprocess.run")
def test_record_sections_runs_the_task_of_the_video_project(
    mock_run: MagicMock,
) -> None:
    mock_run.return_value = _completed("transcript.toml records ...")

    record_sections(REPO_ROOT)

    assert mock_run.call_args.args[0][-1] == "record-sections"
    assert mock_run.call_args.kwargs["cwd"] == VIDEO_DIR


@patch("functions.concepts_video.subprocess.run")
def test_record_sections_reports_a_task_that_fails(mock_run: MagicMock) -> None:
    mock_run.return_value = _completed(returncode=1, stderr="cannot write each hash")

    with pytest.raises(ReleaseError, match="cannot write each hash"):
        record_sections(REPO_ROOT)


# --- the update of the transcript ---


@pytest.fixture
def claude() -> Iterator[MagicMock]:
    """Stand in for the Claude run and for the em-dash guard around it."""
    with (
        patch("functions.concepts_video.run_claude") as run,
        patch("functions.concepts_video.em_dash_lines", return_value=("before",)),
        patch("functions.concepts_video.reword_added_em_dashes") as reword,
    ):
        run.reword = reword
        yield run


def _answer(*segments: tuple[str, str, str], for_the_reviewer: str = "") -> dict:
    return {
        "segments": [
            {"id": segment_id, "status": status, "reason": reason}
            for segment_id, status, reason in segments
        ],
        "for_the_reviewer": for_the_reviewer,
    }


def test_update_video_hands_claude_each_reported_segment_and_what_changed(
    claude: MagicMock,
) -> None:
    claude.return_value = _answer(
        ("stack", "still_correct", "The video does not show this."),
        ("launchers", "changed", "The flag has a new name."),
        for_the_reviewer="  The composition of `stack` shows `--with`.\n",
    )

    update = update_video(REPO_ROOT, (_CHANGED_SECTION, _DISAGREEING))

    # One outcome per reported segment, in the order the check reported them.
    assert update == VideoUpdate(
        outcomes=(
            SegmentOutcome(
                segment_id="launchers",
                status=SegmentStatus.CHANGED,
                reason="The flag has a new name.",
            ),
            SegmentOutcome(
                segment_id="stack",
                status=SegmentStatus.STILL_CORRECT,
                reason="The video does not show this.",
            ),
        ),
        for_the_reviewer="The composition of `stack` shows `--with`.",
    )
    assert [outcome.segment_id for outcome in update.changed] == ["launchers"]
    prompt = claude.call_args.args[0]
    assert "docs/concepts_video/transcript.toml" in prompt
    assert "### Segment `launchers`" in prompt
    assert (
        "The section `Quickstart` of `docs/src/content/docs/quickstart.mdx` changed."
        in prompt
    )
    assert "```diff\n--- last release\n+++ now\n-Run `--with mujoco`." in prompt
    assert (
        "The section `A new section` of `docs/src/content/docs/guides/new_page.mdx` "
        "changed.\nThe last release has no such section, so all of it is new." in prompt
    )
    assert "### Segment `stack`" in prompt
    assert (
        "A source names a section that does not exist: guides/node_stack.mdx: "
        "no section has the heading `Gone`." in prompt
    )
    assert (
        "No section of the segment holds the technical name `--with`, which "
        "sentence `launch` shows." in prompt
    )


def test_update_video_lets_claude_edit_files_and_run_nothing(
    claude: MagicMock,
) -> None:
    claude.return_value = _answer(("launchers", "still_correct", "Still right."))

    update_video(REPO_ROOT, (_CHANGED_SECTION,))

    options = claude.call_args.kwargs
    # No shell: the run reads and edits files, and the tools that exist for
    # it are the ones it is allowed.
    assert options["tools"] == "Read Edit Grep Glob"
    assert options["allowed_tools"] == "Read Edit Grep Glob"
    assert options["permission_mode"] == "acceptEdits"
    assert options["cwd"] == REPO_ROOT


def test_update_video_holds_the_edits_to_no_em_dash(claude: MagicMock) -> None:
    claude.return_value = _answer(("launchers", "changed", "The flag has a new name."))

    update_video(REPO_ROOT, (_CHANGED_SECTION,))

    # The lines that held an em-dash before the run are the ones the guard
    # does not count as added.
    claude.reword.assert_called_once_with(("before",), REPO_ROOT)


def test_update_video_refuses_to_run_with_no_segment(claude: MagicMock) -> None:
    with pytest.raises(ReleaseError, match="no segment to read"):
        update_video(REPO_ROOT, ())

    claude.assert_not_called()


@pytest.mark.parametrize(
    ("answer", "problem"),
    [
        # A reported segment the run never looked at must not pass as read.
        (_answer(), "covers the segments"),
        (
            _answer(
                ("launchers", "changed", "Edited."), ("opening", "changed", "Edited.")
            ),
            "covers the segments",
        ),
        (
            _answer(
                ("launchers", "changed", "Edited."),
                ("launchers", "still_correct", "Still right."),
            ),
            "names the segment 'launchers' twice",
        ),
        (_answer(("launchers", "rewritten", "Edited.")), "unknown status"),
        ({"segments": "launchers", "for_the_reviewer": ""}, "must be a list"),
        ({"segments": []}, "'for_the_reviewer'"),
        ({"segments": ["launchers"], "for_the_reviewer": ""}, "must be an object"),
        (
            {
                "segments": [{"id": "launchers", "status": "changed"}],
                "for_the_reviewer": "",
            },
            "missing id/reason",
        ),
    ],
)
def test_update_video_refuses_a_report_that_does_not_cover_the_segments(
    claude: MagicMock, answer: dict, problem: str
) -> None:
    claude.return_value = answer

    with pytest.raises(ReleaseError, match=problem):
        update_video(REPO_ROOT, (_CHANGED_SECTION,))


def test_the_video_tasks_are_real_tasks_of_the_video_project() -> None:
    # The release names two tasks of another pixi workspace. A task renamed
    # there would otherwise fail a release at its prepare stage only.
    manifest = Path(__file__).resolve().parents[2] / "docs/concepts_video/pixi.toml"
    tasks = tomllib.loads(manifest.read_text(encoding="utf-8"))["tasks"]

    assert {"docs-check", "record-sections"} <= set(tasks)
