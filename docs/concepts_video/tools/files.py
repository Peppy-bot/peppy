"""Hashes and downloads of the files that the build gets."""

import hashlib
from pathlib import Path
import shutil
import urllib.request

READ_SIZE = 1024 * 1024
# Some servers refuse the client name that urllib gives by default.
CLIENT_NAME = "peppy-concepts-video"


class WrongContent(Exception):
    """A file does not have the SHA-256 that the project records for it."""


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as file:
        while chunk := file.read(READ_SIZE):
            digest.update(chunk)
    return digest.hexdigest()


def refuse_wrong_content(path: Path, sha256: str) -> None:
    found = sha256_of(path)
    if found != sha256:
        raise WrongContent(f"{path} has the SHA-256 {found}, and the project records {sha256}")


def download(url: str, destination: Path, sha256: str) -> None:
    """Get `url` into `destination`, and refuse content with another hash.

    The content goes to a file beside the destination first, so a download
    that stops leaves no file at the destination.
    """
    destination.parent.mkdir(parents=True, exist_ok=True)
    partial = destination.with_name(destination.name + ".part")
    request = urllib.request.Request(url, headers={"User-Agent": CLIENT_NAME})
    with urllib.request.urlopen(request) as response, partial.open("wb") as file:
        shutil.copyfileobj(response, file, READ_SIZE)
    try:
        refuse_wrong_content(partial, sha256)
    except WrongContent:
        partial.unlink()
        raise
    partial.replace(destination)
