"""Verify that release archives contain all required binaries."""

from __future__ import annotations

import struct
import tarfile
from pathlib import Path

from .cli import RELEASE_TRIPLES, ReleaseError

REQUIRED_ALL = [
    "bin/peppy",
    "bin/zenohd",
    "bin/apptainer/bin/apptainer",
]

# ELF e_machine values for the architectures peppy releases for. Linux
# binaries are verified against these; a binary whose header names another
# machine fails the release. The v0.25.3 x86_64 archive shipped an aarch64
# apptainer through the presence-only check, and every consumer of the
# tarball (installs, CI runners) broke at exec with no build-time signal.
ELF_MACHINE_BY_ARCH = {
    "x86_64": 62,
    "aarch64": 183,
}

REQUIRED_MACOS = [
    "bin/lima/bin/limactl",
]

MACOS_TRIPLES = frozenset(t for t in RELEASE_TRIPLES if "apple-darwin" in t)


def _elf_machine(header: bytes) -> int | None:
    """The ELF e_machine of a binary's leading bytes, None when not ELF."""
    if len(header) < 20 or header[:4] != b"\x7fELF":
        return None
    return struct.unpack_from("<H", header, 18)[0]


def verify_release_archive(archive_path: Path, triple: str) -> list[str]:
    """Verify a single .tgz archive contains all required binaries, and that
    each Linux binary is built for the triple's architecture.

    Returns a list of problems (empty if the archive is sound). Presence alone
    is not enough: a binary for the wrong machine sits at the right path and
    fails only at exec on the consumer's host.
    """
    problems: list[str] = []

    required = list(REQUIRED_ALL)
    if triple in MACOS_TRIPLES:
        required.extend(REQUIRED_MACOS)

    expected_machine = (
        None if triple in MACOS_TRIPLES else ELF_MACHINE_BY_ARCH[triple.split("-")[0]]
    )

    with tarfile.open(archive_path, "r:gz") as tar:
        members = {m.name.lstrip("./"): m for m in tar.getmembers()}
        for item in required:
            member = members.get(item)
            if member is None:
                problems.append(f"missing {item}")
                continue
            if expected_machine is None:
                continue
            extracted = tar.extractfile(member)
            if extracted is None:
                problems.append(f"{item} is not a regular file")
                continue
            machine = _elf_machine(extracted.read(20))
            if machine != expected_machine:
                problems.append(
                    f"{item} is built for ELF machine {machine}, "
                    f"the {triple} archive needs {expected_machine}"
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

    for triple in (triples if triples is not None else RELEASE_TRIPLES):
        archive = dist_dir / f"peppy-{triple}.tgz"

        if not archive.exists():
            errors.append(f"{triple}: archive not found at {archive}")
            continue

        for problem in verify_release_archive(archive, triple):
            errors.append(f"{triple}: {problem}")

    if errors:
        summary = "\n  ".join(errors)
        raise ReleaseError(
            f"Release verification failed:\n  {summary}"
        )
