"""Downloads of the release scripts, with the curl options of the build."""

from __future__ import annotations

import hashlib
import subprocess
from pathlib import Path

from .cli import ReleaseError

# The curl options of every download a release makes outside cargo: a
# transient failure (a timeout, a refused connection, an HTTP 5xx such as a
# gateway timeout of GitHub) is retried for up to three minutes, and a
# permanent one fails at once. They are CURL_DOWNLOAD_OPTIONS of
# peppy-shared/build-helpers/src/download.rs, which gives the reasons, and
# scripts/tests/test_download.py keeps the two the same.
CURL_DOWNLOAD_OPTIONS = (
    "--fail --silent --show-error --location"
    " --retry 10 --retry-max-time 180 --retry-connrefused"
)


def download(url: str, destination: Path) -> str:
    """Download *url* to *destination* and return the file's SHA-256."""
    result = subprocess.run(
        ["curl", *CURL_DOWNLOAD_OPTIONS.split(), url, "--output", str(destination)],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise ReleaseError(
            f"failed to download {url} (curl exit {result.returncode}): "
            f"{result.stderr.strip()}"
        )
    with destination.open("rb") as downloaded:
        return hashlib.file_digest(downloaded, "sha256").hexdigest()
