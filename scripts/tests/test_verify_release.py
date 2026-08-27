"""Tests for functions.verify_release module."""

from __future__ import annotations

import io
import struct
import tarfile
from pathlib import Path

import pytest

from functions.cli import RELEASE_TRIPLES, ReleaseError
from functions.verify_release import (
    ELF_MACHINE_BY_ARCH,
    verify_all_releases,
    verify_release_archive,
)


def _elf_bytes(machine: int) -> bytes:
    """A minimal ELF header naming `machine`: magic, padding, e_type, e_machine."""
    return b"\x7fELF" + bytes(12) + b"\x02\x00" + struct.pack("<H", machine)


def _binary_for(triple: str) -> bytes:
    """Content shaped like a binary of the triple's architecture. Darwin
    archives are not arch-checked, so any bytes stand in for Mach-O there."""
    if "apple-darwin" in triple:
        return b"fake mach-o binary"
    return _elf_bytes(ELF_MACHINE_BY_ARCH[triple.split("-")[0]])


def _create_archive(
    path: Path,
    members: list[str],
    triple: str,
    content_overrides: dict[str, bytes] | None = None,
    *,
    uid: int = 0,
    gid: int = 0,
    uname: str = "",
    gname: str = "",
) -> None:
    """Create a .tgz archive whose members carry the triple's binary shape
    and, by default, the zeroed ownership release packaging must produce."""
    overrides = content_overrides or {}
    with tarfile.open(path, "w:gz") as tar:
        for name in members:
            data = overrides.get(name, _binary_for(triple))
            info = tarfile.TarInfo(name=f"./{name}")
            info.size = len(data)
            info.uid = uid
            info.gid = gid
            info.uname = uname
            info.gname = gname
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
    _create_archive(archive, _all_required_members(triple), triple)

    missing = verify_release_archive(archive, triple)
    assert missing == []


def test_verify_archive_missing_zenohd(tmp_path: Path) -> None:
    triple = "x86_64-unknown-linux-gnu"
    members = [m for m in _all_required_members(triple) if m != "bin/zenohd"]
    archive = tmp_path / f"peppy-{triple}.tgz"
    _create_archive(archive, members, triple)

    problems = verify_release_archive(archive, triple)
    assert "missing bin/zenohd" in problems


def test_verify_archive_missing_apptainer(tmp_path: Path) -> None:
    triple = "aarch64-unknown-linux-gnu"
    members = [
        m for m in _all_required_members(triple) if "apptainer" not in m
    ]
    archive = tmp_path / f"peppy-{triple}.tgz"
    _create_archive(archive, members, triple)

    problems = verify_release_archive(archive, triple)
    assert "missing bin/apptainer/bin/apptainer" in problems


def test_verify_lima_only_required_for_macos(tmp_path: Path) -> None:
    triple = "x86_64-unknown-linux-gnu"
    members = _all_required_members(triple)
    # Lima is NOT in the members for Linux — should pass
    archive = tmp_path / f"peppy-{triple}.tgz"
    _create_archive(archive, members, triple)

    missing = verify_release_archive(archive, triple)
    assert missing == []


def test_verify_lima_required_for_macos(tmp_path: Path) -> None:
    triple = "aarch64-apple-darwin"
    members = [m for m in _all_required_members(triple) if "lima" not in m]
    archive = tmp_path / f"peppy-{triple}.tgz"
    _create_archive(archive, members, triple)

    problems = verify_release_archive(archive, triple)
    assert "missing bin/lima/bin/limactl" in problems


def test_verify_all_releases_passes(tmp_path: Path) -> None:
    for triple in RELEASE_TRIPLES:
        archive = tmp_path / f"peppy-{triple}.tgz"
        _create_archive(archive, _all_required_members(triple), triple)

    verify_all_releases(tmp_path)


def test_verify_all_releases_missing_archive(tmp_path: Path) -> None:
    # Create all archives except one
    for triple in RELEASE_TRIPLES[:-1]:
        archive = tmp_path / f"peppy-{triple}.tgz"
        _create_archive(archive, _all_required_members(triple), triple)

    with pytest.raises(ReleaseError, match="archive not found"):
        verify_all_releases(tmp_path)


def test_verify_all_releases_missing_binary(tmp_path: Path) -> None:
    for triple in RELEASE_TRIPLES:
        archive = tmp_path / f"peppy-{triple}.tgz"
        members = _all_required_members(triple)
        # Remove zenohd from one archive
        if triple == "aarch64-unknown-linux-gnu":
            members = [m for m in members if m != "bin/zenohd"]
        _create_archive(archive, members, triple)

    with pytest.raises(ReleaseError, match="missing bin/zenohd"):
        verify_all_releases(tmp_path)


def test_verify_archive_rejects_a_wrong_arch_binary(tmp_path: Path) -> None:
    # The v0.25.3 x86_64 archive shipped an aarch64 apptainer at the right
    # path: presence passed, every consumer failed at exec.
    triple = "x86_64-unknown-linux-gnu"
    archive = tmp_path / f"peppy-{triple}.tgz"
    _create_archive(
        archive,
        _all_required_members(triple),
        triple,
        content_overrides={
            "bin/apptainer/bin/apptainer": _elf_bytes(ELF_MACHINE_BY_ARCH["aarch64"])
        },
    )

    problems = verify_release_archive(archive, triple)
    assert any("apptainer" in p and "ELF machine" in p for p in problems), problems


def test_verify_archive_rejects_a_link_member(tmp_path: Path) -> None:
    # extractfile follows a link member to its target's bytes, so a symlink to
    # a valid ELF would pass the header check; the member's own type is what
    # proves the required path carries a real file.
    triple = "x86_64-unknown-linux-gnu"
    archive = tmp_path / f"peppy-{triple}.tgz"
    with tarfile.open(archive, "w:gz") as tar:
        data = _binary_for(triple)
        for name in ("bin/peppy", "bin/zenohd"):
            info = tarfile.TarInfo(name=f"./{name}")
            info.size = len(data)
            tar.addfile(info, io.BytesIO(data))
        link = tarfile.TarInfo(name="./bin/apptainer/bin/apptainer")
        link.type = tarfile.SYMTYPE
        link.linkname = "../../peppy"
        tar.addfile(link)

    problems = verify_release_archive(archive, triple)
    assert "bin/apptainer/bin/apptainer is not a regular file" in problems


def test_verify_archive_rejects_a_binary_that_is_not_elf(tmp_path: Path) -> None:
    triple = "aarch64-unknown-linux-gnu"
    archive = tmp_path / f"peppy-{triple}.tgz"
    _create_archive(
        archive,
        _all_required_members(triple),
        triple,
        content_overrides={"bin/peppy": b"#!/bin/sh\necho not a binary\n"},
    )

    problems = verify_release_archive(archive, triple)
    assert any("bin/peppy" in p for p in problems), problems


def test_verify_archive_rejects_build_host_ownership(tmp_path: Path) -> None:
    """Members stamped with the packing machine's uid or account names fail.

    GNU tar restores ownership on root installs: a foreign uid aborts the
    install inside user namespaces, and a resolvable user or group name hands
    the tree to a same-named local account.
    """
    triple = "aarch64-unknown-linux-gnu"
    archive = tmp_path / f"peppy-{triple}.tgz"
    _create_archive(
        archive,
        _all_required_members(triple),
        triple,
        uid=501,
        gid=20,
        uname="builder",
        gname="staff",
    )

    problems = verify_release_archive(archive, triple)
    expected = (
        "bin/peppy carries build host ownership "
        "(uid=501, gid=20, uname='builder', gname='staff')"
    )
    assert expected in problems
