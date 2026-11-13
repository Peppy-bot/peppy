"""Small transcripts for the tests."""

from tools.transcript import Transcript, parse_transcript

VOICE = {
    "id": "af_heart",
    "speed": 1.0,
    "language": "en-us",
    "lead_in_seconds": 1.0,
    "sentence_gap_seconds": 0.5,
    "tail_seconds": 2.0,
}


def segment_table(segment_id: str = "opening", sentences: list[dict] | None = None, **changes: object) -> dict:
    """The table of one segment, as the transcript file holds it."""
    table = {
        "id": segment_id,
        "chapter": "A chapter",
        "source": [{"page": "guides/page.mdx", "section": "A section", "sha256": "0" * 64}],
        "voice": sentences if sentences is not None else [{"id": "only", "text": "One sentence."}],
        "screen": {"title": "A title"},
    }
    return {**table, **changes}


def transcript_document(segments: list[dict] | None = None, **changes: object) -> dict:
    """The document of a transcript, as the transcript file holds it."""
    document = {
        "voice": dict(VOICE),
        "pronunciation": {},
        "labels": {"term": "Term"},
        "segment": segments if segments is not None else [segment_table()],
    }
    return {**document, **changes}


def transcript_of(segments: list[dict] | None = None, **changes: object) -> Transcript:
    return parse_transcript(transcript_document(segments, **changes))
