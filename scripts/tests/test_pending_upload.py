"""Tests for functions.pending_upload: the manifest a failed upload leaves behind."""

from __future__ import annotations

import json
import os
import shutil
from pathlib import Path
from unittest.mock import patch

import pytest

from functions.build import BuildArtifact
from functions.cli import ReleaseError
from functions.pending_upload import (
    PendingUpload,
    archive_problems,
    discard_pending_upload,
    load_pending_upload,
    pending_upload_path,
    record_pending_upload,
)
from functions.release_summary import ReleaseContent

RELEASE_COMMIT = "1111111111111111111111111111111111111111"
CONTENT = ReleaseContent(
    title="Topics API hardening",
    description="Hardened the topics API against deadlocks.",
    notes="## What's Changed\n- Fixed topics public API (#265)\n",
)


def _write_archives(dist_dir: Path) -> list[BuildArtifact]:
    dist_dir.mkdir(parents=True, exist_ok=True)
    artifacts: list[BuildArtifact] = []
    for triple in ("aarch64-apple-darwin", "x86_64-unknown-linux-gnu"):
        asset_name = f"peppy-{triple}.tgz"
        asset_path = dist_dir / asset_name
        asset_path.write_bytes(f"archive for {triple}".encode())
        artifacts.append(BuildArtifact(asset_name, asset_path, triple))
    return artifacts


def _record(tmp_path: Path) -> tuple[Path, PendingUpload, list[BuildArtifact]]:
    manifest_path = tmp_path / "dist" / "pending-upload.json"
    artifacts = _write_archives(tmp_path / "dist")
    pending = record_pending_upload(
        manifest_path, "v0.1.0", RELEASE_COMMIT, CONTENT, artifacts
    )
    return manifest_path, pending, artifacts


def test_pending_upload_path_sits_in_the_default_dist_dir(tmp_path: Path) -> None:
    with patch.dict(os.environ, {}, clear=False):
        os.environ.pop("PEPPY_DIST_DIR", None)
        assert pending_upload_path(tmp_path) == tmp_path / "dist" / "pending-upload.json"


def test_pending_upload_path_follows_peppy_dist_dir(tmp_path: Path) -> None:
    custom = tmp_path / "elsewhere"
    with patch.dict(os.environ, {"PEPPY_DIST_DIR": str(custom)}):
        assert pending_upload_path(tmp_path) == custom / "pending-upload.json"


def test_record_then_load_round_trips(tmp_path: Path) -> None:
    manifest_path, recorded, artifacts = _record(tmp_path)

    assert recorded.tag == "v0.1.0"
    assert recorded.release_commit == RELEASE_COMMIT
    assert recorded.content == CONTENT
    assert recorded.artifacts == artifacts
    assert load_pending_upload(manifest_path) == recorded


def test_record_pins_each_archive_digest(tmp_path: Path) -> None:
    _, recorded, artifacts = _record(tmp_path)

    # Two different archives never share a digest, and each digest is the
    # SHA-256 hex of that archive's bytes.
    digests = [archive.sha256 for archive in recorded.archives]
    assert len(set(digests)) == len(artifacts)
    assert all(len(digest) == 64 for digest in digests)


def test_record_rejects_an_archive_outside_the_manifest_dir(tmp_path: Path) -> None:
    artifacts = _write_archives(tmp_path / "elsewhere")
    manifest_path = tmp_path / "dist" / "pending-upload.json"

    with pytest.raises(ReleaseError, match="is not in the manifest's directory"):
        record_pending_upload(manifest_path, "v0.1.0", RELEASE_COMMIT, CONTENT, artifacts)

    assert not manifest_path.exists()


def test_manifest_names_archives_relative_to_itself(tmp_path: Path) -> None:
    manifest_path, _, artifacts = _record(tmp_path)

    # No absolute path is written: the directory has to work wherever it is
    # copied, on another machine included.
    text = manifest_path.read_text()
    assert str(tmp_path) not in text
    assert "asset_path" not in text
    for artifact in artifacts:
        assert artifact.asset_name in text


def test_a_copied_dist_dir_loads_and_verifies_from_its_new_place(
    tmp_path: Path,
) -> None:
    manifest_path, recorded, _ = _record(tmp_path)
    moved_dir = tmp_path / "other-machine" / "staging"
    shutil.copytree(manifest_path.parent, moved_dir)

    pending = load_pending_upload(moved_dir / "pending-upload.json")

    assert pending is not None
    assert pending.tag == recorded.tag
    assert pending.release_commit == recorded.release_commit
    assert pending.content == recorded.content
    assert [a.asset_path for a in pending.artifacts] == [
        moved_dir / a.asset_name for a in recorded.artifacts
    ]
    assert archive_problems(pending) == []


def test_load_returns_none_without_a_manifest(tmp_path: Path) -> None:
    assert load_pending_upload(tmp_path / "dist" / "pending-upload.json") is None


def test_load_rejects_malformed_json(tmp_path: Path) -> None:
    manifest_path = tmp_path / "pending-upload.json"
    manifest_path.write_text("{not json")

    with pytest.raises(ReleaseError) as excinfo:
        load_pending_upload(manifest_path)

    # The message names the file and how to move on.
    message = str(excinfo.value)
    assert str(manifest_path) in message
    assert "Delete it and retry" in message


@pytest.mark.parametrize(
    ("mutate", "reason"),
    [
        (lambda data: data.pop("tag"), "'tag' must be a non-empty string"),
        (lambda data: data.update(tag=""), "'tag' must be a non-empty string"),
        (lambda data: data.pop("release_commit"), "'release_commit'"),
        (lambda data: data.pop("content"), "'content' must be an object"),
        (lambda data: data["content"].pop("notes"), "'notes' must be a non-empty string"),
        (lambda data: data.update(archives=[]), "'archives' must be a non-empty list"),
        (lambda data: data.update(archives="none"), "'archives' must be a non-empty list"),
        (lambda data: data["archives"][0].pop("sha256"), "'sha256'"),
        (lambda data: data["archives"].append(7), "'archive' must be an object"),
    ],
    ids=[
        "missing-tag",
        "empty-tag",
        "missing-commit",
        "missing-content",
        "missing-notes",
        "empty-archives",
        "archives-not-a-list",
        "archive-without-digest",
        "archive-not-an-object",
    ],
)
def test_load_rejects_a_manifest_missing_a_field(
    tmp_path: Path, mutate, reason: str
) -> None:
    manifest_path, _, _ = _record(tmp_path)
    data = json.loads(manifest_path.read_text())
    mutate(data)
    manifest_path.write_text(json.dumps(data))

    with pytest.raises(ReleaseError, match=reason):
        load_pending_upload(manifest_path)


def test_load_rejects_a_manifest_that_is_not_an_object(tmp_path: Path) -> None:
    manifest_path = tmp_path / "pending-upload.json"
    manifest_path.write_text("[]")

    with pytest.raises(ReleaseError, match="'manifest' must be an object"):
        load_pending_upload(manifest_path)


def test_archive_problems_is_empty_while_the_archives_are_intact(
    tmp_path: Path,
) -> None:
    _, pending, _ = _record(tmp_path)

    assert archive_problems(pending) == []


def test_archive_problems_reports_a_missing_archive(tmp_path: Path) -> None:
    _, pending, artifacts = _record(tmp_path)
    artifacts[0].asset_path.unlink()

    assert archive_problems(pending) == [f"{artifacts[0].asset_name} is missing"]


def test_archive_problems_reports_a_rewritten_archive(tmp_path: Path) -> None:
    _, pending, artifacts = _record(tmp_path)
    artifacts[1].asset_path.write_bytes(b"rebuilt with another tag")

    assert archive_problems(pending) == [
        f"{artifacts[1].asset_name} changed since it was built"
    ]


def test_archive_problems_reports_every_archive(tmp_path: Path) -> None:
    _, pending, artifacts = _record(tmp_path)
    artifacts[0].asset_path.unlink()
    artifacts[1].asset_path.write_bytes(b"rebuilt")

    assert archive_problems(pending) == [
        f"{artifacts[0].asset_name} is missing",
        f"{artifacts[1].asset_name} changed since it was built",
    ]


def test_discard_removes_the_manifest_and_keeps_the_archives(tmp_path: Path) -> None:
    manifest_path, _, artifacts = _record(tmp_path)

    discard_pending_upload(manifest_path)

    assert not manifest_path.exists()
    assert all(artifact.asset_path.is_file() for artifact in artifacts)


def test_discard_tolerates_a_missing_manifest(tmp_path: Path) -> None:
    discard_pending_upload(tmp_path / "pending-upload.json")
