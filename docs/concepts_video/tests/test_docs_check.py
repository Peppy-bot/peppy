"""The document that the docs check prints for the release."""

from tools import checks, docs_check, upload


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
