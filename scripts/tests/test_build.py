"""Tests for functions.build module."""

from __future__ import annotations

import os
import tarfile
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from functions.build import (
    build_and_package,
    find_build_dir,
    find_peppy_binary,
    find_zenohd_binary,
    package_release,
)
from functions.cli import RELEASE_TRIPLES, ReleaseError


def test_release_triples_contains_expected_values() -> None:
    expected = (
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
    )
    assert RELEASE_TRIPLES == expected


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
        tmp_path
        / "target"
        / triple
        / "release"
        / "build"
        / "pmi-abc123"
        / "out"
        / "zenohd"
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
                triple,
                tmp_path,
                "containers-*/out/apptainer-install",
                "apptainer install",
            )


def test_package_release_creates_valid_tarball(tmp_path: Path) -> None:
    triple = "aarch64-apple-darwin"
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

    with tarfile.open(artifact.asset_path, "r:gz") as tar:
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
    """Verify the tarball layout matches what install.sh expects."""
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


def test_build_and_package_rejects_unsupported_triple(tmp_path: Path) -> None:
    with pytest.raises(ReleaseError, match="unsupported target"):
        build_and_package("v0.1.0", "wasm32-unknown-unknown", tmp_path)


@patch("functions.build.package_release")
@patch("functions.build.find_build_dir")
@patch("functions.build.find_zenohd_binary")
@patch("functions.build.find_peppy_binary")
@patch("functions.build.cargo_build")
def test_build_and_package_native_calls_cargo_build(
    mock_cargo: MagicMock,
    mock_peppy: MagicMock,
    mock_zenohd: MagicMock,
    mock_build_dir: MagicMock,
    mock_package: MagicMock,
    tmp_path: Path,
) -> None:
    mock_peppy.return_value = tmp_path / "peppy"
    mock_zenohd.return_value = tmp_path / "zenohd"
    mock_build_dir.return_value = tmp_path / "apptainer"
    mock_package.return_value = MagicMock()

    build_and_package("v0.1.0", "aarch64-unknown-linux-gnu", tmp_path)

    mock_cargo.assert_called_once_with(
        "v0.1.0", "aarch64-unknown-linux-gnu", tmp_path
    )


@patch("functions.build.package_release")
@patch("functions.build.find_build_dir")
@patch("functions.build.find_zenohd_binary")
@patch("functions.build.find_peppy_binary")
@patch("functions.lima.cargo_build_in_lima")
def test_build_and_package_with_lima_calls_lima_build(
    mock_lima_build: MagicMock,
    mock_peppy: MagicMock,
    mock_zenohd: MagicMock,
    mock_build_dir: MagicMock,
    mock_package: MagicMock,
    tmp_path: Path,
) -> None:
    limactl = tmp_path / "limactl"
    limactl.write_bytes(b"fake")
    mock_peppy.return_value = tmp_path / "peppy"
    mock_zenohd.return_value = tmp_path / "zenohd"
    mock_build_dir.return_value = tmp_path / "apptainer"
    mock_package.return_value = MagicMock()

    build_and_package(
        "v0.1.0", "x86_64-unknown-linux-gnu", tmp_path, limactl=limactl
    )

    mock_lima_build.assert_called_once_with(
        limactl, "v0.1.0", "x86_64-unknown-linux-gnu", tmp_path
    )
