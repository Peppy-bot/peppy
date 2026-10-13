"""Verify that release archives are sound and free of build machine identity."""

from __future__ import annotations

import struct
import tarfile
from dataclasses import dataclass
from pathlib import Path
from typing import IO

from .build import release_archive_name
from .cli import RELEASE_TRIPLES, ReleaseError

REQUIRED_ALL = [
    "bin/peppy",
    "bin/zenohd",
    "bin/apptainer/bin/apptainer",
]

# ELF e_machine values for the architectures peppy releases for. Linux
# binaries are verified against these; a binary whose header names another
# machine fails the release. Presence alone does not catch such a binary: it
# sits at the right path and breaks every consumer of the tarball (installs,
# CI runners) at exec, with no build-time signal.
ELF_MACHINE_BY_ARCH = {
    "x86_64": 62,
    "aarch64": 183,
}

# Raspberry Pi OS runs the Raspberry Pi 5 on a kernel with 16 KiB pages. The
# kernel and the dynamic loader map each LOAD segment of an ELF file in whole
# pages, so a segment aligned to less than a page, or whose file offset and
# address fall at different positions in a page, cannot be mapped there. Every
# aarch64 ELF file of a Linux archive must allow 16 KiB pages.
AARCH64_PAGE_SIZE = 16 * 1024

REQUIRED_MACOS = [
    "bin/lima/bin/limactl",
]

MACOS_TRIPLES = frozenset(t for t in RELEASE_TRIPLES if "apple-darwin" in t)

_ELF_MAGIC = b"\x7fELF"
# e_ident[EI_CLASS] and e_ident[EI_DATA] of the one kind of ELF file a Linux
# archive holds: 64-bit, little-endian.
_ELFCLASS64 = 2
_ELFDATA2LSB = 1
# The ELF64 file header: e_ident, e_type, e_machine, e_version, e_entry,
# e_phoff, e_shoff, e_flags, e_ehsize, e_phentsize, e_phnum, e_shentsize,
# e_shnum, e_shstrndx.
_ELF64_HEADER = struct.Struct("<16sHHIQQQIHHHHHH")
# An ELF64 program header: p_type, p_flags, p_offset, p_vaddr, p_paddr,
# p_filesz, p_memsz, p_align.
_ELF64_PROGRAM_HEADER = struct.Struct("<IIQQQQQQ")
_PT_LOAD = 1


class MalformedElfError(Exception):
    """A file that starts with the ELF magic but is not an ELF file this
    verification can read."""


@dataclass(frozen=True)
class LoadSegment:
    """A PT_LOAD program header: a part of the file the kernel maps."""

    offset: int
    address: int
    alignment: int


@dataclass(frozen=True)
class Elf:
    """What the verification reads of a 64-bit little-endian ELF file."""

    machine: int
    load_segments: tuple[LoadSegment, ...]


def _read_exactly(stream: IO[bytes], size: int, what: str) -> bytes:
    data = stream.read(size)
    if len(data) != size:
        raise MalformedElfError(f"the file ends inside its {what}")
    return data


def read_elf(stream: IO[bytes]) -> Elf | None:
    """Read the ELF header and the program headers at the start of *stream*.

    Returns None when the stream does not start with the ELF magic. Reads
    forward only, so it works on the member streams of a compressed tar
    stream, which cannot seek back.
    """
    magic = stream.read(len(_ELF_MAGIC))
    if magic != _ELF_MAGIC:
        return None
    header = magic + _read_exactly(
        stream, _ELF64_HEADER.size - len(magic), "ELF header"
    )
    (
        ident,
        _e_type,
        machine,
        _version,
        _entry,
        program_headers_offset,
        _section_headers_offset,
        _flags,
        _header_size,
        program_header_size,
        program_header_count,
        _section_header_size,
        _section_header_count,
        _section_names_index,
    ) = _ELF64_HEADER.unpack(header)
    if ident[4] != _ELFCLASS64 or ident[5] != _ELFDATA2LSB:
        raise MalformedElfError("it is not a 64-bit little-endian ELF file")
    if program_header_count and program_header_size != _ELF64_PROGRAM_HEADER.size:
        raise MalformedElfError(
            f"its program headers are {program_header_size} bytes, "
            f"not {_ELF64_PROGRAM_HEADER.size}"
        )
    if program_header_count and program_headers_offset < _ELF64_HEADER.size:
        raise MalformedElfError(
            f"its program headers start at {program_headers_offset}, "
            "inside the ELF header"
        )

    load_segments: list[LoadSegment] = []
    if program_header_count:
        _read_exactly(
            stream, program_headers_offset - _ELF64_HEADER.size, "ELF header gap"
        )
        table = _read_exactly(
            stream,
            program_header_count * _ELF64_PROGRAM_HEADER.size,
            "program headers",
        )
        for (
            p_type,
            _p_flags,
            offset,
            address,
            _p_paddr,
            _filesz,
            _memsz,
            alignment,
        ) in _ELF64_PROGRAM_HEADER.iter_unpack(table):
            if p_type == _PT_LOAD:
                load_segments.append(LoadSegment(offset, address, alignment))
    return Elf(machine=machine, load_segments=tuple(load_segments))


def _page_size_problem(name: str, elf: Elf) -> str | None:
    """Why a kernel with 16 KiB pages cannot map *elf*, or None when it can.

    Names the first LOAD segment it cannot map.
    """
    for segment in elf.load_segments:
        if segment.alignment < AARCH64_PAGE_SIZE:
            return (
                f"{name} aligns a LOAD segment to {segment.alignment} bytes, "
                f"and Raspberry Pi OS runs the Raspberry Pi 5 with "
                f"{AARCH64_PAGE_SIZE}-byte pages"
            )
        if (segment.address - segment.offset) % AARCH64_PAGE_SIZE:
            return (
                f"{name} has a LOAD segment at file offset {segment.offset:#x} "
                f"and address {segment.address:#x}, which fall at different "
                f"positions in a {AARCH64_PAGE_SIZE}-byte page, the page size "
                "Raspberry Pi OS runs the Raspberry Pi 5 with"
            )
    return None


def verify_release_archive(archive_path: Path, triple: str) -> list[str]:
    """Verify a single .tgz archive contains all required binaries, that each
    Linux binary is built for the triple's architecture, that each aarch64
    ELF file maps on a kernel with 16 KiB pages, and that no member carries
    ownership of the machine that packed it.

    Returns a list of problems (empty if the archive is sound). Presence alone
    is not enough: a binary for the wrong machine sits at the right path and
    fails only at exec on the consumer's host. Ownership is checked because
    GNU tar restores it on root installs, where a foreign uid aborts the
    install inside user namespaces and a resolvable user or group name hands
    the tree to a same-named local account.
    """
    problems: list[str] = []

    required = list(REQUIRED_ALL)
    if triple in MACOS_TRIPLES:
        required.extend(REQUIRED_MACOS)

    arch = triple.split("-")[0]
    expected_machine = None if triple in MACOS_TRIPLES else ELF_MACHINE_BY_ARCH[arch]

    members: dict[str, tarfile.TarInfo] = {}
    elf_files: dict[str, Elf] = {}
    # One pass over the archive as a stream: the gzip stream cannot seek back,
    # so the pass reads the ELF headers of each member as it reaches it. Only
    # the regular files of a Linux archive are read; a link member's own type
    # is what the required paths are checked against below.
    with tarfile.open(archive_path, "r|gz") as tar:
        for member in tar:
            name = member.name.removeprefix("./")
            members[name] = member
            if expected_machine is None or not member.isfile():
                continue
            stream = tar.extractfile(member)
            if stream is None:
                continue
            try:
                elf = read_elf(stream)
            except MalformedElfError as error:
                problems.append(f"{name} is not a readable ELF file: {error}")
                continue
            if elf is not None:
                elf_files[name] = elf

    for item in required:
        member = members.get(item)
        if member is None:
            problems.append(f"missing {item}")
            continue
        if expected_machine is None:
            continue
        if not member.isfile():
            problems.append(f"{item} is not a regular file")
            continue
        elf = elf_files.get(item)
        machine = elf.machine if elf is not None else None
        if machine != expected_machine:
            problems.append(
                f"{item} is built for ELF machine {machine}, "
                f"the {triple} archive needs {expected_machine}"
            )

    if expected_machine == ELF_MACHINE_BY_ARCH["aarch64"]:
        for name, elf in elf_files.items():
            if elf.machine != expected_machine:
                continue
            problem = _page_size_problem(name, elf)
            if problem is not None:
                problems.append(problem)

    for name, member in members.items():
        if member.uid != 0 or member.gid != 0 or member.uname or member.gname:
            problems.append(
                f"{name} carries build host ownership "
                f"(uid={member.uid}, gid={member.gid}, "
                f"uname={member.uname!r}, gname={member.gname!r})"
            )

    return problems


def verify_all_releases(
    dist_dir: Path, triples: tuple[str, ...] | list[str] | None = None
) -> None:
    """Verify release archives exist and contain all required binaries.

    When *triples* is ``None`` (the default), all ``RELEASE_TRIPLES`` are
    checked.  Pass an explicit list to verify only a subset (e.g. when
    building on Linux where only the native target is produced).

    Raises ReleaseError with a summary of every problem found.
    """
    errors: list[str] = []

    for triple in triples if triples is not None else RELEASE_TRIPLES:
        archive = dist_dir / release_archive_name(triple)

        if not archive.exists():
            errors.append(f"{triple}: archive not found at {archive}")
            continue

        for problem in verify_release_archive(archive, triple):
            errors.append(f"{triple}: {problem}")

    if errors:
        summary = "\n  ".join(errors)
        raise ReleaseError(f"Release verification failed:\n  {summary}")
