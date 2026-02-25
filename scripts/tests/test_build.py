"""Tests for functions.build module."""

from __future__ import annotations

import os
import tarfile
from pathlib import Path
from unittest.mock import patch

import pytest

from functions.build import (
    SUPPORTED_TRIPLES,
    BuildArtifact,
    detect_host_triple,
    find_build_dir,
    find_peppy_binary,
    find_zenohd_binary,
    package_release,
)
from functions.cli import ReleaseError


class TestSupportedTriples:
    def test_contains_expected_triples(self) -> None:
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

    def test_is_frozenset(self) -> None:
        assert isinstance(SUPPORTED_TRIPLES, frozenset)


class TestDetectHostTriple:
    def test_valid_triple(self) -> None:
        rustc_output = "rustc 1.82.0 (abc123 2025-01-01)\nbinary: rustc\nhost: aarch64-apple-darwin\nrelease: 1.82.0\n"
        with patch("functions.build.subprocess.run") as mock_run:
            mock_run.return_value.returncode = 0
            mock_run.return_value.stdout = rustc_output
            triple = detect_host_triple()
        assert triple == "aarch64-apple-darwin"

    def test_unsupported_triple(self) -> None:
        rustc_output = "rustc 1.82.0\nbinary: rustc\nhost: wasm32-unknown-unknown\nrelease: 1.82.0\n"
        with patch("functions.build.subprocess.run") as mock_run:
            mock_run.return_value.returncode = 0
            mock_run.return_value.stdout = rustc_output
            with pytest.raises(ReleaseError, match="unsupported host target"):
                detect_host_triple()

    def test_rustc_failure(self) -> None:
        with patch("functions.build.subprocess.run") as mock_run:
            mock_run.return_value.returncode = 1
            mock_run.return_value.stderr = "command not found"
            with pytest.raises(ReleaseError, match="failed to run"):
                detect_host_triple()

    def test_no_host_line(self) -> None:
        with patch("functions.build.subprocess.run") as mock_run:
            mock_run.return_value.returncode = 0
            mock_run.return_value.stdout = "rustc 1.82.0\nbinary: rustc\n"
            with pytest.raises(ReleaseError, match="could not determine"):
                detect_host_triple()


class TestFindPeppyBinary:
    def test_primary_path(self, tmp_path: Path) -> None:
        triple = "aarch64-apple-darwin"
        bin_path = tmp_path / "target" / triple / "release" / "peppy"
        bin_path.parent.mkdir(parents=True)
        bin_path.write_bytes(b"fake binary")

        with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(tmp_path / "target")}):
            result = find_peppy_binary(triple, tmp_path)
        assert result == bin_path

    def test_fallback_path(self, tmp_path: Path) -> None:
        triple = "aarch64-apple-darwin"
        # Primary path doesn't exist, only fallback
        fallback = tmp_path / "target" / "release" / "peppy"
        fallback.parent.mkdir(parents=True)
        fallback.write_bytes(b"fake binary")

        with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(tmp_path / "target")}):
            result = find_peppy_binary(triple, tmp_path)
        assert result == fallback

    def test_not_found(self, tmp_path: Path) -> None:
        triple = "aarch64-apple-darwin"
        (tmp_path / "target").mkdir()

        with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(tmp_path / "target")}):
            with pytest.raises(ReleaseError, match="peppy binary not found"):
                find_peppy_binary(triple, tmp_path)


class TestFindZenohdBinary:
    def test_found(self, tmp_path: Path) -> None:
        triple = "aarch64-apple-darwin"
        zenohd_path = (
            tmp_path / "target" / triple / "release" / "build" / "pmi-abc123" / "out" / "zenohd"
        )
        zenohd_path.parent.mkdir(parents=True)
        zenohd_path.write_bytes(b"fake zenohd")

        with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(tmp_path / "target")}):
            result = find_zenohd_binary(triple, tmp_path)
        assert result == zenohd_path

    def test_not_found(self, tmp_path: Path) -> None:
        triple = "aarch64-apple-darwin"
        build_dir = tmp_path / "target" / triple / "release" / "build"
        build_dir.mkdir(parents=True)

        with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(tmp_path / "target")}):
            with pytest.raises(ReleaseError, match="zenohd binary not found"):
                find_zenohd_binary(triple, tmp_path)


class TestFindBuildDir:
    def test_found(self, tmp_path: Path) -> None:
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

    def test_not_found(self, tmp_path: Path) -> None:
        triple = "aarch64-apple-darwin"
        build_dir = tmp_path / "target" / triple / "release" / "build"
        build_dir.mkdir(parents=True)

        with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(tmp_path / "target")}):
            with pytest.raises(ReleaseError, match="apptainer install directory not found"):
                find_build_dir(
                    triple, tmp_path, "containers-*/out/apptainer-install", "apptainer install"
                )


class TestPackageRelease:
    def test_creates_valid_tarball(self, tmp_path: Path) -> None:
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
            assert any("apptainer" in n for n in member_names)
            assert any("lima" in n for n in member_names)

    def test_respects_peppy_dist_dir(self, tmp_path: Path) -> None:
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
