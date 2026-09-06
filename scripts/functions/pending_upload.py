"""A release whose archives are built but not yet published, kept across runs.

Building the archives is the slow part of a release: the native build plus two
Linux cross-compiles in the Lima VM. Publishing them is a handful of HTTP
requests that fail for reasons unrelated to the archives (a dropped
connection, an exhausted port range, a GitHub outage). This module records
everything the publish step needs next to the archives, so a run that fails
there can be resumed from the upload rather than from the build.

The manifest lives in the dist directory with the archives it describes and
names them relative to itself, so the directory is self-contained: copied to a
checkout of the same commit on another machine, it can be published from there.
The manifest pins the release commit and the SHA-256 of every archive, so a
resume publishes exactly what was built and verified for that commit, never an
archive that a later build (a `--local` one, say) wrote over it.
"""

from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .build import BuildArtifact, release_dist_dir
from .cli import ReleaseError
from .release_summary import ReleaseContent

MANIFEST_NAME = "pending-upload.json"


@dataclass(frozen=True)
class PendingArchive:
    """A built release archive and the digest it had when it was verified."""

    artifact: BuildArtifact
    sha256: str


@dataclass(frozen=True)
class PendingUpload:
    """A release whose archives are built and verified but not yet published."""

    tag: str
    release_commit: str
    content: ReleaseContent
    archives: tuple[PendingArchive, ...]

    @property
    def artifacts(self) -> list[BuildArtifact]:
        return [archive.artifact for archive in self.archives]


def pending_upload_path(repo_root: Path) -> Path:
    """Where the manifest lives: next to the archives, in the dist directory."""
    return release_dist_dir(repo_root) / MANIFEST_NAME


def _sha256_of(path: Path) -> str:
    with open(path, "rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()


def record_pending_upload(
    path: Path,
    tag: str,
    release_commit: str,
    content: ReleaseContent,
    artifacts: list[BuildArtifact],
) -> PendingUpload:
    """Write the manifest describing *artifacts* to *path* and return it.

    Called once the archives are built and verified and before the first
    publish request goes out, so a failure anywhere in the publish step leaves
    a manifest behind for the next run to resume from.

    The archives must sit in the manifest's directory: that is where a load
    looks for them, and what keeps the directory movable as a whole.
    """
    for artifact in artifacts:
        if artifact.asset_path.parent != path.parent:
            raise ReleaseError(
                f"archive '{artifact.asset_path}' is not in the manifest's "
                f"directory '{path.parent}'"
            )
    pending = PendingUpload(
        tag=tag,
        release_commit=release_commit,
        content=content,
        archives=tuple(
            PendingArchive(artifact=artifact, sha256=_sha256_of(artifact.asset_path))
            for artifact in artifacts
        ),
    )
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(_to_json(pending), indent=2) + "\n", encoding="utf-8")
    return pending


def load_pending_upload(path: Path) -> PendingUpload | None:
    """Return the manifest at *path*, or None when there is none.

    A manifest that cannot be parsed is an error rather than a silent miss: the
    release script is its only writer, so a broken one means something else
    touched the dist directory, and the user should look at it before the
    archives are rebuilt over it.
    """
    if not path.is_file():
        return None
    try:
        return _from_json(json.loads(path.read_text(encoding="utf-8")), path.parent)
    except ValueError as e:
        raise ReleaseError(
            f"'{path}' is not a valid pending-upload manifest ({e}). "
            f"Delete it and retry."
        ) from e


def archive_problems(pending: PendingUpload) -> list[str]:
    """Describe every archive that is no longer the one the manifest recorded.

    An archive is only good for a resume when it is byte for byte what was
    built and verified: a missing file or a different digest means something
    wrote to the dist directory since, and the manifest no longer describes
    what is on disk. Returns an empty list when every archive is intact.
    """
    problems: list[str] = []
    for archive in pending.archives:
        path = archive.artifact.asset_path
        if not path.is_file():
            problems.append(f"{archive.artifact.asset_name} is missing")
        elif _sha256_of(path) != archive.sha256:
            problems.append(f"{archive.artifact.asset_name} changed since it was built")
    return problems


def discard_pending_upload(path: Path) -> None:
    """Remove the manifest. The archives it described stay on disk."""
    path.unlink(missing_ok=True)


def _to_json(pending: PendingUpload) -> dict[str, Any]:
    return {
        "tag": pending.tag,
        "release_commit": pending.release_commit,
        "content": {
            "title": pending.content.title,
            "description": pending.content.description,
            "notes": pending.content.notes,
        },
        "archives": [
            {
                "asset_name": archive.artifact.asset_name,
                "target_triple": archive.artifact.target_triple,
                "sha256": archive.sha256,
            }
            for archive in pending.archives
        ],
    }


def _from_json(data: Any, dist_dir: Path) -> PendingUpload:
    """Parse the manifest's JSON into a PendingUpload.

    Archives are named relative to the manifest, so they resolve against
    *dist_dir*, the directory it was read from. Raises ValueError naming the
    first field that is missing or malformed.
    """
    top = _object(data, "manifest")
    content = _object(top.get("content"), "content")
    archives = top.get("archives")
    if not isinstance(archives, list) or not archives:
        raise ValueError("'archives' must be a non-empty list")
    return PendingUpload(
        tag=_text(top, "tag"),
        release_commit=_text(top, "release_commit"),
        content=ReleaseContent(
            title=_text(content, "title"),
            description=_text(content, "description"),
            notes=_text(content, "notes"),
        ),
        archives=tuple(_archive_from_json(entry, dist_dir) for entry in archives),
    )


def _archive_from_json(data: Any, dist_dir: Path) -> PendingArchive:
    entry = _object(data, "archive")
    asset_name = _text(entry, "asset_name")
    return PendingArchive(
        artifact=BuildArtifact(
            asset_name=asset_name,
            asset_path=dist_dir / asset_name,
            target_triple=_text(entry, "target_triple"),
        ),
        sha256=_text(entry, "sha256"),
    )


def _object(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ValueError(f"'{label}' must be an object")
    return value


def _text(obj: dict[str, Any], key: str) -> str:
    value = obj.get(key)
    if not isinstance(value, str) or not value:
        raise ValueError(f"'{key}' must be a non-empty string")
    return value
