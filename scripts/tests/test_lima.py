"""Tests for functions.lima module."""

from __future__ import annotations

import os
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from functions.cli import ReleaseError
from functions.lima import (
    GUEST_CARGO_HOME,
    GUEST_RUSTUP_HOME,
    cargo_build_in_lima,
    ensure_lima_vm,
    find_limactl,
)


def test_find_limactl_from_build_output(tmp_path: Path) -> None:
    triple = "aarch64-apple-darwin"
    limactl = (
        tmp_path
        / "target"
        / triple
        / "release"
        / "build"
        / "containers-abc123"
        / "out"
        / "lima-install"
        / "bin"
        / "limactl"
    )
    limactl.parent.mkdir(parents=True)
    limactl.write_bytes(b"fake limactl")

    with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(tmp_path / "target")}):
        result = find_limactl(tmp_path)
    assert result == limactl


def test_find_limactl_fallback_to_cache(tmp_path: Path) -> None:
    # No build output, but cache exists
    cache_dir = tmp_path / ".peppy" / "tmp" / "lima-2.1.0-Darwin-arm64" / "bin"
    cache_dir.mkdir(parents=True)
    limactl = cache_dir / "limactl"
    limactl.write_bytes(b"cached limactl")

    with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(tmp_path / "target")}), \
         patch("functions.lima.Path.home", return_value=tmp_path):
        (tmp_path / "target").mkdir()
        result = find_limactl(tmp_path)
    assert result == limactl


def test_find_limactl_not_found(tmp_path: Path) -> None:
    with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(tmp_path / "target")}), \
         patch("functions.lima.Path.home", return_value=tmp_path):
        (tmp_path / "target").mkdir()
        with pytest.raises(ReleaseError, match="limactl not found"):
            find_limactl(tmp_path)


def test_ensure_lima_vm_already_running(tmp_path: Path) -> None:
    limactl = tmp_path / "limactl"

    with patch("functions.lima._run_limactl") as mock_run:
        mock_run.return_value = MagicMock(stdout="Running\n")
        ensure_lima_vm(limactl)

    mock_run.assert_called_once()


def test_ensure_lima_vm_starts_stopped_instance(tmp_path: Path) -> None:
    limactl = tmp_path / "limactl"

    with patch("functions.lima._run_limactl") as mock_run:
        # First call: list (Stopped), second call: start (success)
        mock_run.side_effect = [
            MagicMock(stdout="Stopped\n"),
            MagicMock(returncode=0),
        ]
        ensure_lima_vm(limactl)

    assert mock_run.call_count == 2


def test_ensure_lima_vm_not_found_raises(tmp_path: Path) -> None:
    limactl = tmp_path / "limactl"

    with patch("functions.lima._run_limactl") as mock_run:
        mock_run.return_value = MagicMock(stdout="")
        with pytest.raises(ReleaseError, match="not found"):
            ensure_lima_vm(limactl)


def test_cargo_build_in_lima_constructs_correct_command(tmp_path: Path) -> None:
    limactl = tmp_path / "limactl"
    repo_root = tmp_path / "repo"

    with patch("functions.lima._lima_shell") as mock_shell:
        mock_shell.return_value = MagicMock(returncode=0)
        cargo_build_in_lima(
            limactl, "v0.1.0", "aarch64-unknown-linux-gnu", repo_root
        )

    mock_shell.assert_called_once()
    script = mock_shell.call_args[0][1]
    assert f"RUSTUP_HOME={GUEST_RUSTUP_HOME}" in script
    assert f"CARGO_HOME={GUEST_CARGO_HOME}" in script
    assert "PEPPY_GIT_TAG=v0.1.0" in script
    assert 'RUSTC_WRAPPER=""' in script
    assert "--target aarch64-unknown-linux-gnu" in script
    assert "-j 8" in script
    assert str(repo_root) in script


def test_cargo_build_in_lima_sets_cross_linker_for_x86_64(tmp_path: Path) -> None:
    limactl = tmp_path / "limactl"
    repo_root = tmp_path / "repo"

    with patch("functions.lima._lima_shell") as mock_shell:
        mock_shell.return_value = MagicMock(returncode=0)
        cargo_build_in_lima(
            limactl, "v0.1.0", "x86_64-unknown-linux-gnu", repo_root
        )

    script = mock_shell.call_args[0][1]
    assert "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc" in script


def test_cargo_build_in_lima_no_cross_linker_for_aarch64(tmp_path: Path) -> None:
    limactl = tmp_path / "limactl"
    repo_root = tmp_path / "repo"

    with patch("functions.lima._lima_shell") as mock_shell:
        mock_shell.return_value = MagicMock(returncode=0)
        cargo_build_in_lima(
            limactl, "v0.1.0", "aarch64-unknown-linux-gnu", repo_root
        )

    script = mock_shell.call_args[0][1]
    assert "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER" not in script
    assert "CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_GNU_LINKER" not in script


def test_cargo_build_in_lima_raises_on_failure(tmp_path: Path) -> None:
    limactl = tmp_path / "limactl"
    repo_root = tmp_path / "repo"

    with patch("functions.lima._lima_shell") as mock_shell:
        mock_shell.return_value = MagicMock(returncode=1)
        with pytest.raises(ReleaseError, match="cargo build for .* failed in Lima VM"):
            cargo_build_in_lima(
                limactl, "v0.1.0", "x86_64-unknown-linux-gnu", repo_root
            )
