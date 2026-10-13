"""Tests for functions.verify_release module."""

from __future__ import annotations

import io
import struct
import tarfile
from pathlib import Path

import pytest

from functions.cli import RELEASE_TRIPLES, ReleaseError
from functions.verify_release import (
    AARCH64_PAGE_SIZE,
    ELF_MACHINE_BY_ARCH,
    verify_all_releases,
    verify_release_archive,
)

PT_LOAD = 1
PT_NOTE = 4
# What the release builds give every LOAD segment of an aarch64 file.
RELEASE_LOAD_ALIGNMENT = 0x10000


def _elf_bytes(
    machine: int,
    program_headers: tuple[tuple[int, int, int, int], ...] = (
        (PT_LOAD, 0, 0, RELEASE_LOAD_ALIGNMENT),
    ),
    *,
    elf_class: int = 2,
) -> bytes:
    """A minimal little-endian ELF file of *machine*: the ELF64 file header,
    then one program header per (p_type, p_offset, p_vaddr, p_align)."""
    ident = b"\x7fELF" + bytes([elf_class, 1, 1]) + bytes(9)
    header = struct.pack(
        "<16sHHIQQQIHHHHHH",
        ident,
        3,  # e_type: ET_DYN
        machine,
        1,  # e_version
        0,  # e_entry
        64,  # e_phoff: the program headers follow this header
        0,  # e_shoff
        0,  # e_flags
        64,  # e_ehsize
        56,  # e_phentsize
        len(program_headers),
        64,  # e_shentsize
        0,  # e_shnum
        0,  # e_shstrndx
    )
    table = b"".join(
        struct.pack("<IIQQQQQQ", p_type, 5, offset, address, address, 0, 0, align)
        for p_type, offset, address, align in program_headers
    )
    return header + table


def _binary_for(triple: str) -> bytes:
    """Content shaped like a binary of the triple's architecture. Darwin
    archives are not arch-checked, so any bytes stand in for Mach-O there."""
    if "apple-darwin" in triple:
        return b"fake mach-o binary"
    return _elf_bytes(ELF_MACHINE_BY_ARCH[triple.split("-")[0]])


def _add_member(
    tar: tarfile.TarFile,
    name: str,
    data: bytes,
    *,
    uid: int = 0,
    gid: int = 0,
    uname: str = "",
    gname: str = "",
) -> None:
    """Add a regular file member, by default with the zeroed ownership."""
    info = tarfile.TarInfo(name=name)
    info.size = len(data)
    info.uid = uid
    info.gid = gid
    info.uname = uname
    info.gname = gname
    tar.addfile(info, io.BytesIO(data))


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
            _add_member(
                tar, f"./{name}", data, uid=uid, gid=gid, uname=uname, gname=gname
            )


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


@pytest.mark.parametrize(
    ("triple", "left_out"),
    [
        ("x86_64-unknown-linux-gnu", "bin/zenohd"),
        ("aarch64-unknown-linux-gnu", "bin/apptainer/bin/apptainer"),
        ("aarch64-apple-darwin", "bin/lima/bin/limactl"),
    ],
)
def test_verify_archive_reports_a_missing_binary(
    tmp_path: Path, triple: str, left_out: str
) -> None:
    members = [m for m in _all_required_members(triple) if m != left_out]
    archive = tmp_path / f"peppy-{triple}.tgz"
    _create_archive(archive, members, triple)

    problems = verify_release_archive(archive, triple)
    assert f"missing {left_out}" in problems


def test_verify_lima_only_required_for_macos(tmp_path: Path) -> None:
    triple = "x86_64-unknown-linux-gnu"
    members = _all_required_members(triple)
    # A Linux archive holds no Lima, and passes without it.
    archive = tmp_path / f"peppy-{triple}.tgz"
    _create_archive(archive, members, triple)

    missing = verify_release_archive(archive, triple)
    assert missing == []


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
    # An apptainer for another architecture at the right path passes a
    # presence check and fails at exec on every consumer of the archive.
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
            _add_member(tar, f"./{name}", data)
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


def test_verify_archive_rejects_a_binary_that_is_not_elf64(tmp_path: Path) -> None:
    triple = "aarch64-unknown-linux-gnu"
    archive = tmp_path / f"peppy-{triple}.tgz"
    _create_archive(
        archive,
        _all_required_members(triple),
        triple,
        content_overrides={
            "bin/zenohd": _elf_bytes(ELF_MACHINE_BY_ARCH["aarch64"], elf_class=1)
        },
    )

    problems = verify_release_archive(archive, triple)
    assert (
        "bin/zenohd is not a readable ELF file: "
        "it is not a 64-bit little-endian ELF file"
    ) in problems


def test_verify_archive_rejects_a_truncated_elf(tmp_path: Path) -> None:
    triple = "x86_64-unknown-linux-gnu"
    archive = tmp_path / f"peppy-{triple}.tgz"
    _create_archive(
        archive,
        _all_required_members(triple),
        triple,
        content_overrides={"bin/peppy": _binary_for(triple)[:70]},
    )

    problems = verify_release_archive(archive, triple)
    assert (
        "bin/peppy is not a readable ELF file: the file ends inside its program headers"
    ) in problems


@pytest.mark.parametrize("alignment", [AARCH64_PAGE_SIZE, RELEASE_LOAD_ALIGNMENT])
def test_verify_archive_accepts_aarch64_files_that_map_on_16k_pages(
    tmp_path: Path, alignment: int
) -> None:
    triple = "aarch64-unknown-linux-gnu"
    machine = ELF_MACHINE_BY_ARCH["aarch64"]
    # Two segments as a linker lays them out: the second one at a file offset
    # and an address that sit at the same position in a page.
    elf = _elf_bytes(
        machine,
        (
            (PT_LOAD, 0, 0, alignment),
            (PT_LOAD, 0x2340, alignment * 3 + 0x2340, alignment),
        ),
    )
    archive = tmp_path / f"peppy-{triple}.tgz"
    _create_archive(
        archive,
        [*_all_required_members(triple), "bin/apptainer/lib/libseccomp.so.2"],
        triple,
        content_overrides={"bin/apptainer/lib/libseccomp.so.2": elf},
    )

    assert verify_release_archive(archive, triple) == []


def test_verify_archive_reads_only_the_load_segments(tmp_path: Path) -> None:
    # A note or the stack header of a real binary is aligned to a few bytes;
    # the kernel maps no page for it.
    triple = "aarch64-unknown-linux-gnu"
    elf = _elf_bytes(
        ELF_MACHINE_BY_ARCH["aarch64"],
        ((PT_NOTE, 0x238, 0x238, 4), (PT_LOAD, 0, 0, RELEASE_LOAD_ALIGNMENT)),
    )
    archive = tmp_path / f"peppy-{triple}.tgz"
    _create_archive(
        archive,
        _all_required_members(triple),
        triple,
        content_overrides={"bin/peppy": elf},
    )

    assert verify_release_archive(archive, triple) == []


@pytest.mark.parametrize(
    "name",
    ["bin/peppy", "bin/apptainer/libexec/apptainer/bin/starter"],
)
def test_verify_archive_rejects_an_aarch64_file_aligned_to_4k_pages(
    tmp_path: Path, name: str
) -> None:
    # Every ELF file of the archive counts, not only the required binaries:
    # the Raspberry Pi 5 kernel maps each one the archive runs.
    triple = "aarch64-unknown-linux-gnu"
    elf = _elf_bytes(
        ELF_MACHINE_BY_ARCH["aarch64"],
        ((PT_LOAD, 0, 0, RELEASE_LOAD_ALIGNMENT), (PT_LOAD, 0x1000, 0x11000, 0x1000)),
    )
    archive = tmp_path / f"peppy-{triple}.tgz"
    _create_archive(
        archive,
        [*_all_required_members(triple), "bin/apptainer/libexec/apptainer/bin/starter"],
        triple,
        content_overrides={name: elf},
    )

    problems = verify_release_archive(archive, triple)
    assert problems == [
        f"{name} aligns a LOAD segment to 4096 bytes, and Raspberry Pi OS runs "
        "the Raspberry Pi 5 with 16384-byte pages"
    ]


def test_verify_archive_rejects_a_load_segment_off_the_16k_page_grid(
    tmp_path: Path,
) -> None:
    triple = "aarch64-unknown-linux-gnu"
    elf = _elf_bytes(
        ELF_MACHINE_BY_ARCH["aarch64"],
        ((PT_LOAD, 0x1000, 0x10000, RELEASE_LOAD_ALIGNMENT),),
    )
    archive = tmp_path / f"peppy-{triple}.tgz"
    _create_archive(
        archive,
        _all_required_members(triple),
        triple,
        content_overrides={"bin/zenohd": elf},
    )

    problems = verify_release_archive(archive, triple)
    assert problems == [
        "bin/zenohd has a LOAD segment at file offset 0x1000 and address "
        "0x10000, which fall at different positions in a 16384-byte page, the "
        "page size Raspberry Pi OS runs the Raspberry Pi 5 with"
    ]


def test_verify_archive_leaves_x86_64_page_alignment_alone(tmp_path: Path) -> None:
    # Every x86_64 Linux kernel runs 4 KiB pages.
    triple = "x86_64-unknown-linux-gnu"
    elf = _elf_bytes(ELF_MACHINE_BY_ARCH["x86_64"], ((PT_LOAD, 0, 0, 0x1000),))
    archive = tmp_path / f"peppy-{triple}.tgz"
    _create_archive(
        archive,
        _all_required_members(triple),
        triple,
        content_overrides={"bin/peppy": elf},
    )

    assert verify_release_archive(archive, triple) == []


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


def test_verify_archive_names_a_dot_file_in_full(tmp_path: Path) -> None:
    triple = "x86_64-unknown-linux-gnu"
    archive = tmp_path / f"peppy-{triple}.tgz"
    with tarfile.open(archive, "w:gz") as tar:
        for name in _all_required_members(triple):
            _add_member(tar, f"./{name}", _binary_for(triple))
        _add_member(tar, "./.marker", b"x", uid=1000)

    problems = verify_release_archive(archive, triple)
    expected = (
        ".marker carries build host ownership (uid=1000, gid=0, uname='', gname='')"
    )
    assert problems == [expected]


def test_verify_archive_ignores_a_binary_outside_the_archive_root(
    tmp_path: Path,
) -> None:
    # A member under ../ points outside the archive root (GNU tar skips it),
    # so it does not stand in for the required binary of the same path.
    triple = "x86_64-unknown-linux-gnu"
    archive = tmp_path / f"peppy-{triple}.tgz"
    with tarfile.open(archive, "w:gz") as tar:
        _add_member(tar, "../bin/peppy", _binary_for(triple))
        for name in ("bin/zenohd", "bin/apptainer/bin/apptainer"):
            _add_member(tar, f"./{name}", _binary_for(triple))

    problems = verify_release_archive(archive, triple)
    assert problems == ["missing bin/peppy"]
