"""The docs check as a command, with the commands that record the sections and the upload."""

import json
from pathlib import Path
import sys

import pytest

from tools import checks, docs_check, paths, record_sections, record_upload, upload


def test_the_report_holds_the_findings_and_the_state_of_the_upload() -> None:
    findings = [
        checks.SegmentFindings(
            segment_id="opening",
            changed_sections=(checks.ChangedSection(page="guides/page.mdx", section="A section", diff="-old\n+new"),),
            missing_sections=("guides/page.mdx: no section has the heading `Gone`",),
            missing_names=(checks.MissingName(where="sentence `launch`", name="--fast"),),
        )
    ]
    recorded = upload.Upload(youtube_id="FJT02TlbguM", content_sha256="a" * 64)

    assert docs_check.report(findings, "b" * 64, recorded) == {
        "segments": [
            {
                "segment_id": "opening",
                "changed_sections": [{"page": "guides/page.mdx", "section": "A section", "diff": "-old\n+new"}],
                "missing_sections": ["guides/page.mdx: no section has the heading `Gone`"],
                "missing_names": [{"where": "sentence `launch`", "name": "--fast"}],
            }
        ],
        "content_sha256": "b" * 64,
        "upload_is_current": False,
    }


def test_the_report_of_a_transcript_that_the_upload_shows_says_so() -> None:
    recorded = upload.Upload(youtube_id="FJT02TlbguM", content_sha256="a" * 64)

    assert docs_check.report([], "a" * 64, recorded) == {
        "segments": [],
        "content_sha256": "a" * 64,
        "upload_is_current": True,
    }


# --- the commands ---

PAGE = """---
title: A page
---

## A section

Run `peppy stack launch` now.
"""

TRANSCRIPT = """[voice]
id = "af_heart"
speed = 1.0
language = "en-us"
lead_in_seconds = 1.0
sentence_gap_seconds = 0.5
tail_seconds = 2.0

[labels]
term = "Term"

[[segment]]
id = "opening"
chapter = "A chapter"

[[segment.source]]
page = "guides/page.mdx"
section = "A section"
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"

[[segment.voice]]
id = "only"
text = "Run `peppy stack launch`."

[segment.screen]
title = "A title"
"""


@pytest.fixture
def project(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """A video project and its docs pages in a directory of the test.

    The transcript does not record the text of its section, and the record of
    the upload holds the hash of another transcript.
    """
    docs_root = tmp_path / "docs"
    (docs_root / "guides").mkdir(parents=True)
    (docs_root / "guides" / "page.mdx").write_text(PAGE, encoding="utf-8")
    (tmp_path / "transcript.toml").write_text(TRANSCRIPT, encoding="utf-8")
    upload.write_upload(tmp_path / "upload.json", upload.Upload(youtube_id="FJT02TlbguM", content_sha256="a" * 64))
    monkeypatch.setattr(paths, "DOCS_ROOT", docs_root)
    monkeypatch.setattr(paths, "TRANSCRIPT", tmp_path / "transcript.toml")
    monkeypatch.setattr(paths, "UPLOAD", tmp_path / "upload.json")
    return tmp_path


def run(command, monkeypatch: pytest.MonkeyPatch, *arguments: str) -> None:
    monkeypatch.setattr(sys, "argv", ["command", *arguments])
    command.main()


def test_the_docs_check_stops_with_each_problem_of_the_transcript(project: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    with pytest.raises(SystemExit) as stopped:
        run(docs_check, monkeypatch)

    assert "the text of the section `A section` of guides/page.mdx changed" in str(stopped.value)


def test_the_docs_check_passes_when_the_transcript_records_the_sections(
    project: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    run(record_sections, monkeypatch)
    recorded = capsys.readouterr().out

    run(docs_check, monkeypatch)

    assert "transcript.toml records the present text of each section: 1 hashes changed." in recorded
    assert capsys.readouterr().out == "The transcript agrees with the docs.\n"


def test_the_docs_check_passes_on_a_section_that_has_the_text_of_the_baseline(
    project: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    run(docs_check, monkeypatch, "--baseline", str(project / "docs"))

    assert capsys.readouterr().out == "The transcript agrees with the docs.\n"


def test_the_docs_check_refuses_a_baseline_that_is_not_a_directory(project: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    with pytest.raises(SystemExit, match="is not a directory"):
        run(docs_check, monkeypatch, "--baseline", str(project / "no_such_directory"))


def test_the_docs_check_prints_its_report_for_the_release_and_does_not_stop(
    project: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    run(docs_check, monkeypatch, "--json")

    document = json.loads(capsys.readouterr().out)
    assert [segment["segment_id"] for segment in document["segments"]] == ["opening"]
    assert document["segments"][0]["changed_sections"] == [{"page": "guides/page.mdx", "section": "A section", "diff": None}]
    assert document["content_sha256"] == upload.content_sha256(project / "transcript.toml")
    assert document["upload_is_current"] is False


def test_the_record_of_an_upload_holds_its_id_and_the_hash_of_the_transcript(
    project: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    run(record_upload, monkeypatch, "https://youtu.be/AbCdEfGhIjK?si=share")

    recorded = upload.read_upload(project / "upload.json")
    assert recorded.youtube_id == "AbCdEfGhIjK"
    assert upload.out_of_date_problems(recorded, upload.content_sha256(project / "transcript.toml")) == []
    assert "upload.json records the video `AbCdEfGhIjK`" in capsys.readouterr().out


def test_the_record_of_an_upload_refuses_a_link_that_names_no_video(project: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    before = (project / "upload.json").read_text(encoding="utf-8")

    with pytest.raises(SystemExit, match="is not a link of a video on YouTube"):
        run(record_upload, monkeypatch, "https://example.com/video")

    assert (project / "upload.json").read_text(encoding="utf-8") == before
