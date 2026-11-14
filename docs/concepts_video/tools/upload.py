"""The record of the upload: the video on YouTube and the transcript that it shows.

`upload.json` holds the id of the video on YouTube, which the quickstart page
of the docs shows, and the hash of the content of the transcript that this
video was rendered from. The content of the transcript is the transcript
without the sources of its segments: what the video says and what it shows.
`pixi run record-upload <link>` writes the record after an upload.
"""

from dataclasses import dataclass
import hashlib
import json
from pathlib import Path
import re
import tomllib
from typing import Mapping
from urllib.parse import parse_qs, urlparse

from tools.transcript import SHA256, parse_transcript

YOUTUBE_ID = re.compile(r"^[A-Za-z0-9_-]{11}$")
# The host of the short links, where the id is the path.
SHORT_LINK_HOST = "youtu.be"
YOUTUBE_HOSTS = ("youtube.com", "youtube-nocookie.com")
# The paths of YouTube that hold the id of a video as their next part.
PATHS_WITH_AN_ID = frozenset({"embed", "live", "shorts", "video"})


class UploadError(Exception):
    """`upload.json` or a link does not have the form that this module reads."""


@dataclass(frozen=True)
class Upload:
    youtube_id: str
    # The hash of the content of the transcript that the video shows.
    content_sha256: str


def read_upload(path: Path) -> Upload:
    document = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(document, dict):
        raise UploadError(f"{path.name}: it is not an object")
    youtube_id = document.get("youtube_id")
    content_sha256 = document.get("content_sha256")
    if not isinstance(youtube_id, str) or not YOUTUBE_ID.match(youtube_id):
        raise UploadError(f"{path.name}: `youtube_id` is not the id of a video on YouTube, which has 11 characters")
    if not isinstance(content_sha256, str) or not SHA256.match(content_sha256):
        raise UploadError(f"{path.name}: `content_sha256` is not a SHA-256 in lower-case hexadecimal")
    return Upload(youtube_id=youtube_id, content_sha256=content_sha256)


def write_upload(path: Path, upload: Upload) -> None:
    document = {"youtube_id": upload.youtube_id, "content_sha256": upload.content_sha256}
    path.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")


def content_sha256(transcript_path: Path) -> str:
    """The hash of what the transcript makes the video say and show."""
    with transcript_path.open("rb") as file:
        document = tomllib.load(file)
    parse_transcript(document)
    return content_sha256_of_document(document)


def content_sha256_of_document(document: Mapping[str, object]) -> str:
    """The hash of a transcript document without the sources of its segments.

    The form of the file, its comments and the sources do not change the hash.
    """
    content = {
        **document,
        "segment": [{key: value for key, value in segment.items() if key != "source"} for segment in document["segment"]],
    }
    canonical = json.dumps(content, sort_keys=True, ensure_ascii=False, separators=(",", ":"))
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()


def youtube_id_of(link: str) -> str:
    """The id of the video that a link of YouTube names. An id alone is its own link."""
    link = link.strip()
    if YOUTUBE_ID.match(link):
        return link
    address = urlparse(link if "://" in link else f"https://{link}")
    host = (address.hostname or "").removeprefix("www.")
    path_parts = [part for part in address.path.split("/") if part]
    candidates: list[str] = []
    if host == SHORT_LINK_HOST:
        candidates = path_parts[:1]
    elif host in YOUTUBE_HOSTS or host.endswith(tuple(f".{name}" for name in YOUTUBE_HOSTS)):
        candidates = parse_qs(address.query).get("v", []) + [
            following for part, following in zip(path_parts, path_parts[1:]) if part in PATHS_WITH_AN_ID
        ]
    for candidate in candidates:
        if YOUTUBE_ID.match(candidate):
            return candidate
    raise UploadError(f"`{link}` is not a link of a video on YouTube, such as https://youtu.be/<id>")


def out_of_date_problems(upload: Upload, present_content_sha256: str) -> list[str]:
    """Why the video on YouTube is not the video of the transcript."""
    if upload.content_sha256 == present_content_sha256:
        return []
    return [
        f"The video `{upload.youtube_id}` on YouTube shows another transcript. Build the video and upload it, "
        "then run `pixi run record-upload <link of the upload>`, which writes upload.json: "
        f"the id of the upload, and the hash {present_content_sha256} of the content of this transcript."
    ]
