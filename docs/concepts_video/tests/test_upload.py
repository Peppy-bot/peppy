"""The record of the upload."""

from pathlib import Path

import pytest

from tests.factories import segment_table, transcript_document
from tools import upload


@pytest.mark.parametrize(
    "link",
    [
        "FJT02TlbguM",
        "https://youtu.be/FJT02TlbguM",
        "https://youtu.be/FJT02TlbguM?si=abcdefgh",
        "youtu.be/FJT02TlbguM",
        "https://www.youtube.com/watch?v=FJT02TlbguM&t=12s",
        "https://youtube.com/watch?feature=shared&v=FJT02TlbguM",
        "https://m.youtube.com/watch?v=FJT02TlbguM",
        "https://www.youtube.com/embed/FJT02TlbguM",
        "https://www.youtube-nocookie.com/embed/FJT02TlbguM",
        "https://www.youtube.com/shorts/FJT02TlbguM",
        "https://www.youtube.com/live/FJT02TlbguM",
        "https://studio.youtube.com/video/FJT02TlbguM/edit",
        "  https://youtu.be/FJT02TlbguM\n",
    ],
)
def test_each_form_of_a_link_of_youtube_gives_the_id_of_its_video(link: str) -> None:
    assert upload.youtube_id_of(link) == "FJT02TlbguM"


@pytest.mark.parametrize(
    "link",
    [
        "",
        "https://youtu.be/",
        "https://youtu.be/short",
        "https://www.youtube.com/watch?list=PL1234567890",
        "https://www.youtube.com/@peppy",
        "https://vimeo.com/FJT02TlbguM",
        "https://notyoutube.com/watch?v=FJT02TlbguM",
    ],
)
def test_a_link_that_names_no_video_of_youtube_is_refused(link: str) -> None:
    with pytest.raises(upload.UploadError, match="is not a link of a video on YouTube"):
        upload.youtube_id_of(link)


def test_the_content_hash_does_not_change_with_the_sources_of_the_segments() -> None:
    recorded = transcript_document([segment_table(source=[{"page": "a.mdx", "section": "A", "sha256": "0" * 64}])])
    with_other_sources = transcript_document([segment_table(source=[{"page": "b.mdx", "section": "B", "sha256": "1" * 64}])])

    assert upload.content_sha256_of_document(recorded) == upload.content_sha256_of_document(with_other_sources)


@pytest.mark.parametrize(
    "changed",
    [
        transcript_document([segment_table(sentences=[{"id": "only", "text": "Another sentence."}])]),
        transcript_document([segment_table(screen={"title": "Another title"})]),
        transcript_document([segment_table(chapter="Another chapter")]),
        transcript_document(labels={"term": "Another word"}),
        transcript_document(pronunciation={"uv": "you vee"}),
    ],
    ids=["voice", "screen", "chapter", "labels", "pronunciation"],
)
def test_the_content_hash_changes_with_what_the_video_says_and_shows(changed: dict) -> None:
    assert upload.content_sha256_of_document(changed) != upload.content_sha256_of_document(transcript_document())


def test_a_record_with_the_hash_of_the_transcript_has_no_problem() -> None:
    recorded = upload.Upload(youtube_id="FJT02TlbguM", content_sha256="a" * 64)

    assert upload.out_of_date_problems(recorded, "a" * 64) == []


def test_a_record_with_another_hash_names_the_video_and_the_hash_to_record() -> None:
    recorded = upload.Upload(youtube_id="FJT02TlbguM", content_sha256="a" * 64)

    (problem,) = upload.out_of_date_problems(recorded, "b" * 64)

    assert "The video `FJT02TlbguM` on YouTube shows another transcript" in problem
    assert "pixi run record-upload" in problem
    assert "b" * 64 in problem


def test_the_record_that_the_command_writes_reads_back(tmp_path: Path) -> None:
    recorded = upload.Upload(youtube_id="FJT02TlbguM", content_sha256="a" * 64)

    upload.write_upload(tmp_path / "upload.json", recorded)

    assert upload.read_upload(tmp_path / "upload.json") == recorded


@pytest.mark.parametrize(
    ("document", "problem"),
    [
        ("[]", "it is not an object"),
        ('{"youtube_id": "", "content_sha256": "' + "a" * 64 + '"}', "`youtube_id` is not the id of a video"),
        ('{"youtube_id": "https://youtu.be/FJT02TlbguM", "content_sha256": "' + "a" * 64 + '"}', "`youtube_id` is not the id"),
        ('{"youtube_id": "FJT02TlbguM"}', "`content_sha256` is not a SHA-256"),
        ('{"youtube_id": "FJT02TlbguM", "content_sha256": "ABC"}', "`content_sha256` is not a SHA-256"),
    ],
)
def test_a_record_that_does_not_have_its_form_is_refused(tmp_path: Path, document: str, problem: str) -> None:
    (tmp_path / "upload.json").write_text(document, encoding="utf-8")

    with pytest.raises(upload.UploadError, match=problem):
        upload.read_upload(tmp_path / "upload.json")
