"""Verify that release archives contain all required binaries."""

from __future__ import annotations

import tarfile
from pathlib import Path

from .cli import RELEASE_TRIPLES, ReleaseError

REQUIRED_ALL = [
    "bin/peppy",
    "bin/zenohd",
    "bin/apptainer/bin/apptainer",
]

REQUIRED_MACOS = [
    "bin/lima/bin/limactl",
]

MACOS_TRIPLES = frozenset(t for t in RELEASE_TRIPLES if "apple-darwin" in t)


def verify_release_archive(archive_path: Path, triple: str) -> list[str]:
    """Verify a single .tgz archive contains all required binaries.

    Returns a list of missing items (empty if all present).
    """
    missing: list[str] = []

    with tarfile.open(archive_path, "r:gz") as tar:
        member_names = {m.name.lstrip("./") for m in tar.getmembers()}

    required = list(REQUIRED_ALL)
    if triple in MACOS_TRIPLES:
        required.extend(REQUIRED_MACOS)

    for item in required:
        if item not in member_names:
            missing.append(item)

    return missing


def verify_all_releases(dist_dir: Path) -> None:
    """Verify all expected release archives exist and contain all required binaries.

    Raises ReleaseError with a summary of all missing items.
    """
    errors: list[str] = []

    for triple in RELEASE_TRIPLES:
        archive = dist_dir / f"peppy-{triple}.tgz"

        if not archive.exists():
            errors.append(f"{triple}: archive not found at {archive}")
            continue

        missing = verify_release_archive(archive, triple)
        for item in missing:
            errors.append(f"{triple}: missing {item}")

    if errors:
        summary = "\n  ".join(errors)
        raise ReleaseError(
            f"Release verification failed:\n  {summary}"
        )
