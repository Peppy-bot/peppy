"""Tests for functions.verify_release module."""

from __future__ import annotations

import tarfile
from pathlib import Path

import pytest

from functions.cli import RELEASE_TRIPLES, ReleaseError
from functions.verify_release import verify_all_releases, verify_release_archive


def _create_archive(
    path: Path,
    members: list[str],
) -> None:
    """Create a .tgz archive with the given member paths (files with dummy content)."""
    with tarfile.open(path, "w:gz") as tar:
        for name in members:
            import io

            data = b"fake binary"
            info = tarfile.TarInfo(name=f"./{name}")
            info.size = len(data)
            tar.addfile(info, io.BytesIO(data))


def _all_required_members(triple: str) -> list[str]:
    """Return all required member paths for a given triple."""
    members = ["bin/peppy", "bin/zenohd", "bin/apptainer/bin/apptainer"]
    if "apple-darwin" in triple:
        members.append("bin/lima/bin/limactl")
    return members


def test_verify_archive_all_present(tmp_path: Path) -> None:
    triple = "aarch64-apple-darwin"
    archive = tmp_path / f"peppy-{triple}.tgz"
    _create_archive(archive, _all_required_members(triple))

    missing = verify_release_archive(archive, triple)
    assert missing == []


def test_verify_archive_missing_zenohd(tmp_path: Path) -> None:
    triple = "x86_64-unknown-linux-gnu"
    members = [m for m in _all_required_members(triple) if m != "bin/zenohd"]
    archive = tmp_path / f"peppy-{triple}.tgz"
    _create_archive(archive, members)

    missing = verify_release_archive(archive, triple)
    assert "bin/zenohd" in missing


def test_verify_archive_missing_apptainer(tmp_path: Path) -> None:
    triple = "aarch64-unknown-linux-gnu"
    members = [
        m for m in _all_required_members(triple) if "apptainer" not in m
    ]
    archive = tmp_path / f"peppy-{triple}.tgz"
    _create_archive(archive, members)

    missing = verify_release_archive(archive, triple)
    assert "bin/apptainer/bin/apptainer" in missing


def test_verify_lima_only_required_for_macos(tmp_path: Path) -> None:
    triple = "x86_64-unknown-linux-gnu"
    members = _all_required_members(triple)
    # Lima is NOT in the members for Linux — should pass
    archive = tmp_path / f"peppy-{triple}.tgz"
    _create_archive(archive, members)

    missing = verify_release_archive(archive, triple)
    assert missing == []


def test_verify_lima_required_for_macos(tmp_path: Path) -> None:
    triple = "aarch64-apple-darwin"
    members = [m for m in _all_required_members(triple) if "lima" not in m]
    archive = tmp_path / f"peppy-{triple}.tgz"
    _create_archive(archive, members)

    missing = verify_release_archive(archive, triple)
    assert "bin/lima/bin/limactl" in missing


def test_verify_all_releases_passes(tmp_path: Path) -> None:
    for triple in RELEASE_TRIPLES:
        archive = tmp_path / f"peppy-{triple}.tgz"
        _create_archive(archive, _all_required_members(triple))

    verify_all_releases(tmp_path)


def test_verify_all_releases_missing_archive(tmp_path: Path) -> None:
    # Create all archives except one
    for triple in RELEASE_TRIPLES[:-1]:
        archive = tmp_path / f"peppy-{triple}.tgz"
        _create_archive(archive, _all_required_members(triple))

    with pytest.raises(ReleaseError, match="archive not found"):
        verify_all_releases(tmp_path)


def test_verify_all_releases_missing_binary(tmp_path: Path) -> None:
    for triple in RELEASE_TRIPLES:
        archive = tmp_path / f"peppy-{triple}.tgz"
        members = _all_required_members(triple)
        # Remove zenohd from one archive
        if triple == "aarch64-unknown-linux-gnu":
            members = [m for m in members if m != "bin/zenohd"]
        _create_archive(archive, members)

    with pytest.raises(ReleaseError, match="missing bin/zenohd"):
        verify_all_releases(tmp_path)
