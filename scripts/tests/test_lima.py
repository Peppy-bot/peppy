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
    LIMA_INSTANCE,
    LIMA_TEMPLATE,
    LIMA_VERSION,
    RELEASE_PLATFORM_SO,
    SO_BUILD_STATE_MARKER,
    cargo_build_in_lima,
    ensure_lima_vm,
    find_limactl,
    require_prebuilt_peppylib_so,
    stop_lima_vm,
)


def _populate_so_dir(base: Path) -> Path:
    """Create a peppylib .so dir holding the full release set plus the marker."""
    so_dir = base / "so"
    so_dir.mkdir(parents=True)
    for name in (*RELEASE_PLATFORM_SO, SO_BUILD_STATE_MARKER):
        (so_dir / name).write_bytes(b"x")
    return so_dir


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

    with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(tmp_path / "target")}), \
         patch("functions.lima.Path.home", return_value=tmp_path / "empty-home"):
        result = find_limactl(tmp_path)
    assert result == limactl


def test_find_limactl_fallback_to_cache(tmp_path: Path) -> None:
    # No build output, but the exact pinned cache exists. A lexicographically
    # newer unrelated cache must never win release-tool selection.
    cache_dir = (
        tmp_path / ".peppy" / "tmp" / f"lima-{LIMA_VERSION}-Darwin-arm64" / "bin"
    )
    cache_dir.mkdir(parents=True)
    limactl = cache_dir / "limactl"
    limactl.write_bytes(b"cached limactl")
    distractor = tmp_path / ".peppy" / "tmp" / "lima-99.0.0-Darwin-arm64" / "bin"
    distractor.mkdir(parents=True)
    (distractor / "limactl").write_bytes(b"wrong limactl")
    stale_target = (
        tmp_path
        / "target"
        / "aarch64-apple-darwin"
        / "release"
        / "build"
        / "containers-old"
        / "out"
        / "lima-install"
        / "bin"
    )
    stale_target.mkdir(parents=True)
    (stale_target / "limactl").write_bytes(b"stale limactl")

    with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(tmp_path / "target")}), \
         patch("functions.lima.Path.home", return_value=tmp_path):
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


def test_ensure_lima_vm_creates_when_not_found(tmp_path: Path) -> None:
    limactl = tmp_path / "limactl"

    with patch("functions.lima._run_limactl") as mock_run:
        # First call: list (empty = not found), second call: start/create (success)
        mock_run.side_effect = [
            MagicMock(stdout=""),
            MagicMock(returncode=0),
        ]
        ensure_lima_vm(limactl)

    assert mock_run.call_count == 2
    create_call = mock_run.call_args_list[1]
    assert create_call[0] == (
        limactl,
        [
            "start",
            f"--name={LIMA_INSTANCE}",
            "--tty=false",
            "--mount-writable",
            "--containerd=none",
            "--memory=12",
            LIMA_TEMPLATE,
        ],
    )
    assert create_call[1] == {"capture": False}


def test_ensure_lima_vm_create_failure_raises(tmp_path: Path) -> None:
    limactl = tmp_path / "limactl"

    with patch("functions.lima._run_limactl") as mock_run:
        # First call: list (empty = not found), second call: create fails
        mock_run.side_effect = [
            MagicMock(stdout=""),
            MagicMock(returncode=1),
        ]
        with pytest.raises(ReleaseError, match="failed to create Lima VM"):
            ensure_lima_vm(limactl)


def test_cargo_build_in_lima_constructs_correct_command(tmp_path: Path) -> None:
    limactl = tmp_path / "limactl"
    repo_root = tmp_path / "repo"
    so_dir = _populate_so_dir(tmp_path)

    with patch("functions.lima._lima_shell") as mock_shell, \
         patch("functions.lima._prebuilt_peppylib_so_dir", return_value=so_dir):
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
    # The in-VM build embeds the host-built bindings straight from this dir.
    assert f"PEPPYLIB_PREBUILT_SO_DIR={so_dir}" in script
    assert "--target aarch64-unknown-linux-gnu" in script
    assert "-j 8" in script
    assert str(repo_root) in script


def test_cargo_build_in_lima_sets_cross_linker_for_x86_64(tmp_path: Path) -> None:
    limactl = tmp_path / "limactl"
    repo_root = tmp_path / "repo"
    so_dir = _populate_so_dir(tmp_path)

    with patch("functions.lima._lima_shell") as mock_shell, \
         patch("functions.lima._prebuilt_peppylib_so_dir", return_value=so_dir):
        mock_shell.return_value = MagicMock(returncode=0)
        cargo_build_in_lima(
            limactl, "v0.1.0", "x86_64-unknown-linux-gnu", repo_root
        )

    script = mock_shell.call_args[0][1]
    assert "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc" in script


def test_cargo_build_in_lima_no_cross_linker_for_aarch64(tmp_path: Path) -> None:
    limactl = tmp_path / "limactl"
    repo_root = tmp_path / "repo"
    so_dir = _populate_so_dir(tmp_path)

    with patch("functions.lima._lima_shell") as mock_shell, \
         patch("functions.lima._prebuilt_peppylib_so_dir", return_value=so_dir):
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
    so_dir = _populate_so_dir(tmp_path)

    with patch("functions.lima._lima_shell") as mock_shell, \
         patch("functions.lima._prebuilt_peppylib_so_dir", return_value=so_dir):
        mock_shell.return_value = MagicMock(returncode=1)
        with pytest.raises(ReleaseError, match="cargo build for .* failed in Lima VM"):
            cargo_build_in_lima(
                limactl, "v0.1.0", "x86_64-unknown-linux-gnu", repo_root
            )


def test_require_prebuilt_peppylib_so_returns_dir_when_complete(tmp_path: Path) -> None:
    so_dir = _populate_so_dir(tmp_path)
    with patch("functions.lima._prebuilt_peppylib_so_dir", return_value=so_dir):
        assert require_prebuilt_peppylib_so() == so_dir


def test_require_prebuilt_peppylib_so_raises_when_incomplete(tmp_path: Path) -> None:
    so_dir = tmp_path / "so"
    so_dir.mkdir()
    # Only one platform's binding is present; the rest and the marker are missing.
    (so_dir / RELEASE_PLATFORM_SO[0]).write_bytes(b"x")

    with patch("functions.lima._prebuilt_peppylib_so_dir", return_value=so_dir):
        with pytest.raises(ReleaseError, match="prebuilt peppylib bindings missing"):
            require_prebuilt_peppylib_so()


def test_stop_lima_vm_issues_stop(tmp_path: Path) -> None:
    limactl = tmp_path / "limactl"
    with patch("functions.lima._run_limactl") as mock_run:
        mock_run.return_value = MagicMock(returncode=0)
        stop_lima_vm(limactl)
    mock_run.assert_called_once_with(limactl, ["stop", LIMA_INSTANCE])


def test_stop_lima_vm_best_effort_on_failure(tmp_path: Path) -> None:
    limactl = tmp_path / "limactl"
    with patch("functions.lima._run_limactl") as mock_run:
        mock_run.return_value = MagicMock(returncode=1)
        # Must not raise: cleanup should never mask the build's own outcome.
        stop_lima_vm(limactl)
    mock_run.assert_called_once()


def test_cargo_build_in_lima_requires_prebuilt_so(tmp_path: Path) -> None:
    limactl = tmp_path / "limactl"
    repo_root = tmp_path / "repo"
    so_dir = tmp_path / "so"  # never created: every binding is missing

    with patch("functions.lima._prebuilt_peppylib_so_dir", return_value=so_dir), \
         patch("functions.lima._lima_shell") as mock_shell:
        with pytest.raises(ReleaseError, match="prebuilt peppylib bindings missing"):
            cargo_build_in_lima(
                limactl, "v0.1.0", "x86_64-unknown-linux-gnu", repo_root
            )

    # The VM build must not start when the host bindings are absent.
    mock_shell.assert_not_called()
