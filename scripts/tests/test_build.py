"""Tests for functions.build module."""

from __future__ import annotations

import os
import tarfile
from pathlib import Path
from unittest.mock import patch

import pytest

from functions.build import (
    SUPPORTED_TRIPLES,
    detect_host_triple,
    find_build_dir,
    find_peppy_binary,
    find_zenohd_binary,
    package_release,
)
from functions.cli import ReleaseError


def test_supported_triples_contains_expected_triples() -> None:
    expected = {
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-gnu",
        "aarch64-unknown-linux-musl",
        "armv7-unknown-linux-gnueabihf",
        "arm-unknown-linux-gnueabihf",
    }
    assert SUPPORTED_TRIPLES == expected


def test_detect_host_triple_valid_triple() -> None:
    rustc_output = "rustc 1.82.0 (abc123 2025-01-01)\nbinary: rustc\nhost: aarch64-apple-darwin\nrelease: 1.82.0\n"
    with patch("functions.build.subprocess.run") as mock_run:
        mock_run.return_value.returncode = 0
        mock_run.return_value.stdout = rustc_output
        triple = detect_host_triple()
    assert triple == "aarch64-apple-darwin"


def test_detect_host_triple_unsupported_triple() -> None:
    rustc_output = "rustc 1.82.0\nbinary: rustc\nhost: wasm32-unknown-unknown\nrelease: 1.82.0\n"
    with patch("functions.build.subprocess.run") as mock_run:
        mock_run.return_value.returncode = 0
        mock_run.return_value.stdout = rustc_output
        with pytest.raises(ReleaseError, match="unsupported host target"):
            detect_host_triple()


def test_detect_host_triple_rustc_failure() -> None:
    with patch("functions.build.subprocess.run") as mock_run:
        mock_run.return_value.returncode = 1
        mock_run.return_value.stderr = "command not found"
        with pytest.raises(ReleaseError, match="failed to run"):
            detect_host_triple()


def test_detect_host_triple_no_host_line() -> None:
    with patch("functions.build.subprocess.run") as mock_run:
        mock_run.return_value.returncode = 0
        mock_run.return_value.stdout = "rustc 1.82.0\nbinary: rustc\n"
        with pytest.raises(ReleaseError, match="could not determine"):
            detect_host_triple()


def test_find_peppy_binary_primary_path(tmp_path: Path) -> None:
    triple = "aarch64-apple-darwin"
    bin_path = tmp_path / "target" / triple / "release" / "peppy"
    bin_path.parent.mkdir(parents=True)
    bin_path.write_bytes(b"fake binary")

    with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(tmp_path / "target")}):
        result = find_peppy_binary(triple, tmp_path)
    assert result == bin_path


def test_find_peppy_binary_fallback_path(tmp_path: Path) -> None:
    triple = "aarch64-apple-darwin"
    # Primary path doesn't exist, only fallback
    fallback = tmp_path / "target" / "release" / "peppy"
    fallback.parent.mkdir(parents=True)
    fallback.write_bytes(b"fake binary")

    with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(tmp_path / "target")}):
        result = find_peppy_binary(triple, tmp_path)
    assert result == fallback


def test_find_peppy_binary_not_found(tmp_path: Path) -> None:
    triple = "aarch64-apple-darwin"
    (tmp_path / "target").mkdir()

    with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(tmp_path / "target")}):
        with pytest.raises(ReleaseError, match="peppy binary not found"):
            find_peppy_binary(triple, tmp_path)


def test_find_zenohd_binary_found(tmp_path: Path) -> None:
    triple = "aarch64-apple-darwin"
    zenohd_path = (
        tmp_path / "target" / triple / "release" / "build" / "pmi-abc123" / "out" / "zenohd"
    )
    zenohd_path.parent.mkdir(parents=True)
    zenohd_path.write_bytes(b"fake zenohd")

    with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(tmp_path / "target")}):
        result = find_zenohd_binary(triple, tmp_path)
    assert result == zenohd_path


def test_find_zenohd_binary_not_found(tmp_path: Path) -> None:
    triple = "aarch64-apple-darwin"
    build_dir = tmp_path / "target" / triple / "release" / "build"
    build_dir.mkdir(parents=True)

    with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(tmp_path / "target")}):
        with pytest.raises(ReleaseError, match="zenohd binary not found"):
            find_zenohd_binary(triple, tmp_path)


def test_find_build_dir_found(tmp_path: Path) -> None:
    triple = "aarch64-apple-darwin"
    apptainer = (
        tmp_path
        / "target"
        / triple
        / "release"
        / "build"
        / "containers-abc123"
        / "out"
        / "apptainer-install"
    )
    apptainer.mkdir(parents=True)

    with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(tmp_path / "target")}):
        result = find_build_dir(
            triple, tmp_path, "containers-*/out/apptainer-install", "apptainer install"
        )
    assert result == apptainer


def test_find_build_dir_not_found(tmp_path: Path) -> None:
    triple = "aarch64-apple-darwin"
    build_dir = tmp_path / "target" / triple / "release" / "build"
    build_dir.mkdir(parents=True)

    with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(tmp_path / "target")}):
        with pytest.raises(ReleaseError, match="apptainer install directory not found"):
            find_build_dir(
                triple, tmp_path, "containers-*/out/apptainer-install", "apptainer install"
            )


def test_package_release_creates_valid_tarball(tmp_path: Path) -> None:
    triple = "aarch64-apple-darwin"
    repo_root = tmp_path / "repo"
    repo_root.mkdir()

    # Create fake binaries
    peppy_bin = tmp_path / "peppy"
    peppy_bin.write_bytes(b"peppy binary")
    zenohd_bin = tmp_path / "zenohd"
    zenohd_bin.write_bytes(b"zenohd binary")

    # Create fake dependency dirs
    apptainer_dir = tmp_path / "apptainer-install"
    apptainer_dir.mkdir()
    (apptainer_dir / "bin" / "apptainer").parent.mkdir(parents=True)
    (apptainer_dir / "bin" / "apptainer").write_bytes(b"apptainer")

    lima_dir = tmp_path / "lima-install"
    lima_dir.mkdir()
    (lima_dir / "bin" / "limactl").parent.mkdir(parents=True)
    (lima_dir / "bin" / "limactl").write_bytes(b"limactl")

    dist_dir = tmp_path / "dist"
    with patch.dict(os.environ, {"PEPPY_DIST_DIR": str(dist_dir)}):
        artifact = package_release(
            triple, repo_root, peppy_bin, zenohd_bin, apptainer_dir, lima_dir
        )

    assert artifact.asset_name == f"peppy-{triple}.tgz"
    assert artifact.asset_path.exists()
    assert artifact.host_triple == triple

    # Verify tarball contents
    with tarfile.open(artifact.asset_path, "r:gz") as tar:
        names = sorted(tar.getnames())
        assert "./bin" in names or "./bin/peppy" in names
        # Check that essential files are present
        member_names = {m.name for m in tar.getmembers()}
        assert any("bin/peppy" in n for n in member_names)
        assert any("bin/zenohd" in n for n in member_names)
        assert any("bin/apptainer" in n for n in member_names)
        assert any("bin/lima" in n for n in member_names)


def test_package_release_creates_tarball_without_lima(tmp_path: Path) -> None:
    triple = "x86_64-unknown-linux-gnu"
    repo_root = tmp_path / "repo"
    repo_root.mkdir()

    peppy_bin = tmp_path / "peppy"
    peppy_bin.write_bytes(b"peppy binary")
    zenohd_bin = tmp_path / "zenohd"
    zenohd_bin.write_bytes(b"zenohd binary")

    apptainer_dir = tmp_path / "apptainer-install"
    apptainer_dir.mkdir()
    (apptainer_dir / "bin" / "apptainer").parent.mkdir(parents=True)
    (apptainer_dir / "bin" / "apptainer").write_bytes(b"apptainer")

    dist_dir = tmp_path / "dist"
    with patch.dict(os.environ, {"PEPPY_DIST_DIR": str(dist_dir)}):
        artifact = package_release(
            triple, repo_root, peppy_bin, zenohd_bin, apptainer_dir
        )

    with tarfile.open(artifact.asset_path, "r:gz") as tar:
        member_names = {m.name for m in tar.getmembers()}
        assert any("bin/peppy" in n for n in member_names)
        assert any("bin/apptainer" in n for n in member_names)
        assert not any("lima" in n for n in member_names)


def test_package_release_tarball_matches_install_sh(tmp_path: Path) -> None:
    """Verify the tarball layout matches what install.sh expects.

    install.sh does: tar -xzf archive -C $TEMP_DIR, then expects:
      $TEMP_DIR/bin/peppy
      $TEMP_DIR/bin/zenohd          (optional)
      $TEMP_DIR/bin/apptainer/...   (optional)
      $TEMP_DIR/bin/lima/...        (optional, macOS only)
    """
    triple = "aarch64-apple-darwin"
    repo_root = tmp_path / "repo"
    repo_root.mkdir()

    peppy_bin = tmp_path / "peppy"
    peppy_bin.write_bytes(b"peppy binary")
    zenohd_bin = tmp_path / "zenohd"
    zenohd_bin.write_bytes(b"zenohd binary")

    apptainer_dir = tmp_path / "apptainer-install"
    apptainer_dir.mkdir()
    (apptainer_dir / "bin").mkdir(parents=True)
    (apptainer_dir / "bin" / "apptainer").write_bytes(b"apptainer")

    lima_dir = tmp_path / "lima-install"
    lima_dir.mkdir()
    (lima_dir / "bin").mkdir(parents=True)
    (lima_dir / "bin" / "limactl").write_bytes(b"limactl")

    dist_dir = tmp_path / "dist"
    with patch.dict(os.environ, {"PEPPY_DIST_DIR": str(dist_dir)}):
        artifact = package_release(
            triple, repo_root, peppy_bin, zenohd_bin, apptainer_dir, lima_dir
        )

    # Simulate what install.sh does: extract and check expected paths
    extract_dir = tmp_path / "extracted"
    extract_dir.mkdir()
    with tarfile.open(artifact.asset_path, "r:gz") as tar:
        tar.extractall(extract_dir)

    assert (extract_dir / "bin" / "peppy").is_file()
    assert (extract_dir / "bin" / "zenohd").is_file()
    assert (extract_dir / "bin" / "apptainer" / "bin" / "apptainer").is_file()
    assert (extract_dir / "bin" / "lima" / "bin" / "limactl").is_file()


def test_package_release_respects_peppy_dist_dir(tmp_path: Path) -> None:
    triple = "aarch64-apple-darwin"
    custom_dist = tmp_path / "custom_dist"

    peppy_bin = tmp_path / "peppy"
    peppy_bin.write_bytes(b"binary")
    zenohd_bin = tmp_path / "zenohd"
    zenohd_bin.write_bytes(b"binary")

    apptainer_dir = tmp_path / "apptainer-install"
    apptainer_dir.mkdir()
    (apptainer_dir / "bin").mkdir()
    (apptainer_dir / "bin" / "apptainer").write_bytes(b"apptainer")

    lima_dir = tmp_path / "lima-install"
    lima_dir.mkdir()
    (lima_dir / "bin").mkdir()
    (lima_dir / "bin" / "limactl").write_bytes(b"limactl")

    with patch.dict(os.environ, {"PEPPY_DIST_DIR": str(custom_dist)}):
        artifact = package_release(
            triple, tmp_path, peppy_bin, zenohd_bin, apptainer_dir, lima_dir
        )

    assert artifact.asset_path.parent == custom_dist
