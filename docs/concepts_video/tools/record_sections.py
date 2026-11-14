"""Records the present text of the docs sections: `pixi run record-sections`.

The command writes, for each source of the transcript, the hash of the text
that its section has now. Run it after you read each segment against the
sections that the docs check names. The command changes the hashes only.
"""

from pathlib import Path
import re
import tomllib

from tools import paths
from tools.docs_sections import SectionNotFound, read_section
from tools.transcript import Transcript, parse_transcript, read_transcript
from tools.upload import content_sha256_of_document

SEGMENT_HEADER = re.compile(r"^\s*\[\[\s*segment\s*\]\]")
SOURCE_HEADER = re.compile(r"^\s*\[\[\s*segment\s*\.\s*source\s*\]\]")
TABLE_HEADER = re.compile(r"^\s*\[")
HASH_LINE = re.compile(r'^(\s*sha256\s*=\s*")[0-9a-f]{64}(".*)$')


class CannotRecord(Exception):
    """The transcript file does not have the form that this command writes."""


def present_hashes(transcript: Transcript, docs_root: Path) -> list[list[str]]:
    """For each segment in order, the hash of the present text of each of its sources.

    A source that names no section keeps the hash that the transcript records.
    """
    return [[_present_hash(source, docs_root) for source in segment.sources] for segment in transcript.segments]


def with_hashes(transcript_text: str, hashes: list[list[str]]) -> str:
    """The text of the transcript file with `hashes` as the hashes of its sources.

    `hashes` holds, for each segment in order, the hash of each of its sources
    in order. Each other character of the file stays.
    """
    lines = transcript_text.split("\n")
    segment_index = -1
    source_index = -1
    in_source = False
    for index, line in enumerate(lines):
        if SOURCE_HEADER.match(line):
            source_index += 1
            in_source = True
            continue
        if SEGMENT_HEADER.match(line):
            segment_index += 1
            source_index = -1
            in_source = False
            continue
        if TABLE_HEADER.match(line):
            in_source = False
            continue
        hash_line = HASH_LINE.match(line)
        if not (in_source and hash_line):
            continue
        if segment_index >= len(hashes) or source_index >= len(hashes[segment_index]):
            raise CannotRecord("The transcript file has more sources than the transcript.")
        lines[index] = f"{hash_line.group(1)}{hashes[segment_index][source_index]}{hash_line.group(2)}"
    recorded = "\n".join(lines)
    _refuse_another_change(transcript_text, recorded, hashes)
    return recorded


def _present_hash(source, docs_root: Path) -> str:
    try:
        return read_section(docs_root, source.page, source.section).sha256
    except SectionNotFound:
        return source.sha256


def _refuse_another_change(before: str, after: str, hashes: list[list[str]]) -> None:
    """Stop when the new text does not hold each hash, or holds another change."""
    document_before = tomllib.loads(before)
    document_after = tomllib.loads(after)
    written = [[source.sha256 for source in segment.sources] for segment in parse_transcript(document_after).segments]
    if written != hashes:
        raise CannotRecord(
            "The command cannot write each hash: it writes the `sha256` line of a `[[segment.source]]` table only."
        )
    if _without_hashes(document_before) != _without_hashes(document_after):
        raise CannotRecord("The command changed more than the hashes of the sources.")
    if content_sha256_of_document(document_before) != content_sha256_of_document(document_after):
        raise CannotRecord("The command changed the content of the transcript.")


def _without_hashes(document: dict) -> dict:
    segments = [
        {**segment, "source": [{**source, "sha256": ""} for source in segment.get("source", [])]}
        for segment in document["segment"]
    ]
    return {**document, "segment": segments}


def main() -> None:
    transcript = read_transcript(paths.TRANSCRIPT)
    before = paths.TRANSCRIPT.read_text(encoding="utf-8")
    after = with_hashes(before, present_hashes(transcript, paths.DOCS_ROOT))
    paths.TRANSCRIPT.write_text(after, encoding="utf-8")
    changed = sum(1 for old, new in zip(before.split("\n"), after.split("\n")) if old != new)
    print(f"{paths.TRANSCRIPT.name} records the present text of each section: {changed} hashes changed.")


if __name__ == "__main__":
    main()
