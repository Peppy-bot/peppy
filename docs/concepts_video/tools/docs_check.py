"""Checks the transcript against the docs: `pixi run docs-check`.

The command names each segment that does not agree with the sections of the
docs that it names:

- A source names a section that does not exist.
- The text of a section is not the text that the transcript records.
- A technical name of the segment is not in the sections of the segment.

`--baseline <directory>` gives the docs pages of the last release. A section
that has the text of the baseline is not a changed section. `--json` prints
the findings as one JSON document, which the release reads.
"""

import argparse
from dataclasses import asdict
import json
from pathlib import Path

from tools import checks, paths, upload
from tools.transcript import read_transcript


def report(findings: list[checks.SegmentFindings], content_sha256: str, recorded: upload.Upload) -> dict:
    """The findings and the state of the upload, as the document that `--json` prints."""
    return {
        "segments": [_segment_report(found) for found in findings],
        "content_sha256": content_sha256,
        "upload_is_current": not upload.out_of_date_problems(recorded, content_sha256),
    }


def _segment_report(found: checks.SegmentFindings) -> dict:
    return {
        "segment_id": found.segment_id,
        "changed_sections": [asdict(changed) for changed in found.changed_sections],
        "missing_sections": list(found.missing_sections),
        "missing_names": [asdict(missing) for missing in found.missing_names],
    }


def main() -> None:
    parser = argparse.ArgumentParser(description="Check the transcript of the concepts video against the docs.")
    parser.add_argument("--baseline", type=Path, help="the directory of the docs pages of the last release")
    parser.add_argument("--json", action="store_true", help="print the findings as one JSON document")
    arguments = parser.parse_args()
    if arguments.baseline is not None and not arguments.baseline.is_dir():
        raise SystemExit(f"{arguments.baseline} is not a directory.")
    findings = checks.docs_findings(read_transcript(paths.TRANSCRIPT), paths.DOCS_ROOT, arguments.baseline)
    if arguments.json:
        document = report(findings, upload.content_sha256(paths.TRANSCRIPT), upload.read_upload(paths.UPLOAD))
        print(json.dumps(document, indent=2))
        return
    if findings:
        raise SystemExit("\n".join(checks.problems_of(findings)))
    print("The transcript agrees with the docs.")


if __name__ == "__main__":
    main()
