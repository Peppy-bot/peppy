"""Tests for functions.download module."""

from __future__ import annotations

import hashlib
import re
from pathlib import Path

import pytest

from functions.cli import ReleaseError
from functions.download import CURL_DOWNLOAD_OPTIONS, download

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
BUILD_HELPERS_DOWNLOAD = (
    REPO_ROOT / "peppy-shared" / "build-helpers" / "src" / "download.rs"
)


def test_curl_options_are_those_of_the_build_scripts() -> None:
    source = BUILD_HELPERS_DOWNLOAD.read_text()
    match = re.search(r'pub const CURL_DOWNLOAD_OPTIONS: &str =\s*"([^"]*)";', source)
    assert match, f"no CURL_DOWNLOAD_OPTIONS in {BUILD_HELPERS_DOWNLOAD}"
    assert CURL_DOWNLOAD_OPTIONS == match.group(1)


def test_download_writes_the_file_and_returns_its_sha256(tmp_path: Path) -> None:
    source = tmp_path / "go.tar.gz"
    source.write_bytes(b"go toolchain")
    destination = tmp_path / "downloaded.tar.gz"

    digest = download(source.as_uri(), destination)

    assert destination.read_bytes() == b"go toolchain"
    assert digest == hashlib.sha256(b"go toolchain").hexdigest()


def test_download_reports_what_curl_said(tmp_path: Path) -> None:
    missing = (tmp_path / "missing.tar.gz").as_uri()

    with pytest.raises(ReleaseError, match=r"failed to download .*missing\.tar\.gz"):
        download(missing, tmp_path / "downloaded.tar.gz")
