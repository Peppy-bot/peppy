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
    _locked_nodes_shared_rev,
    cargo_build_in_lima,
    ensure_lima_vm,
    find_limactl,
    stage_prebuilt_peppylib_so,
)

# A representative Cargo.lock rev for nodes_shared_code; its 7-char prefix
# `c420252` names the cargo git checkout the staging step targets.
_NODES_SHARED_REV = "c42025204f95702dbdb87112842e33816f440381"


def _write_cargo_lock(repo_root: Path, rev: str = _NODES_SHARED_REV) -> None:
    """Write a minimal Cargo.lock pinning nodes_shared_code at `rev`."""
    repo_root.mkdir(parents=True, exist_ok=True)
    (repo_root / "Cargo.lock").write_text(
        '[[package]]\n'
        'name = "peppylib"\n'
        'version = "0.0.1"\n'
        f'source = "git+https://github.com/Peppy-bot/nodes_shared_code#{rev}"\n'
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
    cache_dir = tmp_path / ".peppy" / "tmp" / "lima-2.1.3-Darwin-arm64" / "bin"
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

    with patch("functions.lima._lima_shell") as mock_shell, \
         patch("functions.lima.stage_prebuilt_peppylib_so"):
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

    with patch("functions.lima._lima_shell") as mock_shell, \
         patch("functions.lima.stage_prebuilt_peppylib_so"):
        mock_shell.return_value = MagicMock(returncode=0)
        cargo_build_in_lima(
            limactl, "v0.1.0", "x86_64-unknown-linux-gnu", repo_root
        )

    script = mock_shell.call_args[0][1]
    assert "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc" in script


def test_cargo_build_in_lima_no_cross_linker_for_aarch64(tmp_path: Path) -> None:
    limactl = tmp_path / "limactl"
    repo_root = tmp_path / "repo"

    with patch("functions.lima._lima_shell") as mock_shell, \
         patch("functions.lima.stage_prebuilt_peppylib_so"):
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

    with patch("functions.lima._lima_shell") as mock_shell, \
         patch("functions.lima.stage_prebuilt_peppylib_so"):
        mock_shell.return_value = MagicMock(returncode=1)
        with pytest.raises(ReleaseError, match="cargo build for .* failed in Lima VM"):
            cargo_build_in_lima(
                limactl, "v0.1.0", "x86_64-unknown-linux-gnu", repo_root
            )


def test_locked_nodes_shared_rev_parses(tmp_path: Path) -> None:
    _write_cargo_lock(tmp_path)
    assert _locked_nodes_shared_rev(tmp_path) == _NODES_SHARED_REV


def test_locked_nodes_shared_rev_missing_raises(tmp_path: Path) -> None:
    (tmp_path / "Cargo.lock").write_text(
        '[[package]]\nname = "other"\nversion = "1.0.0"\n'
    )
    with pytest.raises(ReleaseError, match="no nodes_shared_code git revision"):
        _locked_nodes_shared_rev(tmp_path)


def test_stage_prebuilt_peppylib_so_constructs_command(tmp_path: Path) -> None:
    limactl = tmp_path / "limactl"
    repo_root = tmp_path / "repo"
    _write_cargo_lock(repo_root)
    cargo_home = tmp_path / "host-cargo"

    with patch("functions.lima._lima_shell") as mock_shell, \
         patch.dict(os.environ, {"CARGO_HOME": str(cargo_home)}):
        mock_shell.return_value = MagicMock(returncode=0)
        stage_prebuilt_peppylib_so(limactl, repo_root)

    mock_shell.assert_called_once()
    script = mock_shell.call_args[0][1]
    # Materialises the checkout before seeding it, with the sccache wrapper
    # neutralised so `cargo fetch` does not try to invoke a missing sccache.
    assert 'RUSTC_WRAPPER=""' in script
    assert "cargo fetch --locked" in script
    # Keyed to the locked rev's 7-char prefix, on both source and destination.
    assert "c420252" in script
    assert str(cargo_home) in script
    assert GUEST_CARGO_HOME in script
    assert "peppyos-shared/peppylib-py/peppylib" in script
    # The full release set is required and copied (embed selects at deploy time).
    assert "_peppylib.abi3.linux-aarch64.so" in script
    assert "_peppylib.abi3.linux-x86_64.so" in script
    assert "_peppylib.abi3.macos-aarch64.so" in script
    assert ".so-build-state" in script


def test_stage_prebuilt_peppylib_so_raises_on_failure(tmp_path: Path) -> None:
    limactl = tmp_path / "limactl"
    repo_root = tmp_path / "repo"
    _write_cargo_lock(repo_root)

    with patch("functions.lima._lima_shell") as mock_shell:
        mock_shell.return_value = MagicMock(returncode=4)
        with pytest.raises(
            ReleaseError, match="stage prebuilt peppylib .so into Lima VM"
        ):
            stage_prebuilt_peppylib_so(limactl, repo_root)


def test_cargo_build_in_lima_stages_before_building(tmp_path: Path) -> None:
    limactl = tmp_path / "limactl"
    repo_root = tmp_path / "repo"
    order: list[str] = []

    def record_build(*_args: object, **_kwargs: object) -> MagicMock:
        order.append("build")
        return MagicMock(returncode=0)

    with patch(
        "functions.lima.stage_prebuilt_peppylib_so",
        side_effect=lambda *_a: order.append("stage"),
    ) as mock_stage, patch(
        "functions.lima._lima_shell", side_effect=record_build
    ):
        cargo_build_in_lima(
            limactl, "v0.1.0", "x86_64-unknown-linux-gnu", repo_root
        )

    assert order == ["stage", "build"]
    mock_stage.assert_called_once_with(limactl, repo_root)
