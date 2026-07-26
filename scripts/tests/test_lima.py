"""Tests for functions.lima module."""

from __future__ import annotations

import os
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from functions import lima
from functions.cli import ReleaseError
from functions.lima import (
    GUEST_CARGO_HOME,
    GUEST_COPY_BACK_ARTIFACTS,
    GUEST_RUST_DIR,
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
from .lima_helpers import lima_env


def _populate_so_dir(base: Path) -> Path:
    """Create a peppylib .so dir holding the full release set plus the marker."""
    so_dir = base / "so"
    so_dir.mkdir(parents=True)
    for name in (*RELEASE_PLATFORM_SO, SO_BUILD_STATE_MARKER):
        (so_dir / name).write_bytes(b"x")
    return so_dir


def test_lima_env_honours_short_base_override(tmp_path: Path) -> None:
    with patch.dict(
        os.environ,
        {"PEPPY_TEST_LIMA_HOME": str(tmp_path), "PYTEST_XDIST_WORKER": "gw0"},
    ):
        env = lima_env()

    assert env["LIMA_HOME"] == str(tmp_path / "lti-gw0")


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

    # An empty home, so the cache pin (now checked first) cannot shadow the
    # build-output case this test is about.
    home = tmp_path / "home"
    home.mkdir()

    with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(tmp_path / "target")}), \
         patch("functions.lima.Path.home", return_value=home):
        result = find_limactl(tmp_path)
    assert result == limactl


def test_find_limactl_fallback_to_cache(tmp_path: Path) -> None:
    """The exact pinned cache entry wins over a lexicographic sort.

    Two distractors are planted: a higher-sorting cache directory that the old
    `sorted(...)[-1]` glob would have selected, and a stale build output that
    the old build-output-first ordering would have selected.
    """
    home = tmp_path / "home"
    cache_dir = home / ".peppy" / "tmp" / f"lima-{LIMA_VERSION}-Darwin-arm64" / "bin"
    cache_dir.mkdir(parents=True)
    limactl = cache_dir / "limactl"
    limactl.write_bytes(b"cached limactl")

    distractor = home / ".peppy" / "tmp" / "lima-99.0.0-Darwin-arm64" / "bin"
    distractor.mkdir(parents=True)
    (distractor / "limactl").write_bytes(b"lexicographically later limactl")

    target_dir = tmp_path / "target"
    stale = (
        target_dir
        / "aarch64-apple-darwin"
        / "release"
        / "build"
        / "containers-old"
        / "out"
        / "lima-install"
        / "bin"
    )
    stale.mkdir(parents=True)
    (stale / "limactl").write_bytes(b"stale build output limactl")

    with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(target_dir)}), \
         patch("functions.lima.Path.home", return_value=home):
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

    with patch("functions.lima._run_limactl") as mock_run, \
         patch("functions.lima._vm_resources", return_value=(8, 12)):
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
            "--cpus=8",
            "--memory=12",
            LIMA_TEMPLATE,
        ],
    )
    assert create_call[1] == {"capture": False}


def test_vm_resources_leaves_host_headroom(tmp_path: Path) -> None:
    """A 10-core, 32 GiB host gets 8 vCPUs and 12 GiB (1.5 GiB per vCPU)."""
    with patch("functions.lima.os.cpu_count", return_value=10), \
         patch("functions.lima._host_memory_gib", return_value=32):
        assert lima._vm_resources() == (8, 12)


def test_vm_resources_floors_memory_for_lto_link(tmp_path: Path) -> None:
    """A small host still gets 8 GiB so the serial LTO link is not starved."""
    with patch("functions.lima.os.cpu_count", return_value=4), \
         patch("functions.lima._host_memory_gib", return_value=32):
        assert lima._vm_resources() == (2, 8)


def test_vm_resources_caps_memory_at_half_host_ram(tmp_path: Path) -> None:
    """The guest never claims more than half the host's RAM, even below the floor."""
    with patch("functions.lima.os.cpu_count", return_value=10), \
         patch("functions.lima._host_memory_gib", return_value=8):
        assert lima._vm_resources() == (8, 4)


def test_ensure_lima_vm_create_failure_raises(tmp_path: Path) -> None:
    limactl = tmp_path / "limactl"

    # `_vm_resources` is stubbed for the same reason the success case above
    # stubs it: it reads the host's RAM through `sysctl`, which exists only on
    # macOS, so the real one raises its own ReleaseError on the Linux CI runner
    # before the create call under test is ever reached.
    with patch("functions.lima._run_limactl") as mock_run, \
         patch("functions.lima._vm_resources", return_value=(8, 12)):
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
    target_dir = tmp_path / "target" / ".peppy-release-aarch64-unknown-linux-gnu-abc123"
    so_dir = _populate_so_dir(tmp_path)

    with patch("functions.lima._lima_shell") as mock_shell, \
         patch("functions.lima._prebuilt_peppylib_so_dir", return_value=so_dir):
        mock_shell.return_value = MagicMock(returncode=0)
        cargo_build_in_lima(
            limactl,
            "v0.1.0",
            "aarch64-unknown-linux-gnu",
            repo_root,
            target_dir=target_dir,
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
    # No explicit -j: cargo defaults to the guest's actual vCPU count, which
    # tracks whatever the VM was created with.
    assert "-j" not in script
    assert str(repo_root) in script


def test_cargo_build_in_lima_builds_on_guest_disk_and_copies_back(
    tmp_path: Path,
) -> None:
    """The build must never write cargo output through the virtiofs mount.

    Parallel rustc jobs on virtiofs intermittently lose just-written object
    files, so cargo targets the guest's own disk and only the packaging
    artifacts are copied back to the host-visible target dir.
    """
    limactl = tmp_path / "limactl"
    repo_root = tmp_path / "repo"
    target_dir = tmp_path / "target" / ".peppy-release-aarch64-unknown-linux-gnu-abc123"
    so_dir = _populate_so_dir(tmp_path)

    with patch("functions.lima._lima_shell") as mock_shell, \
         patch("functions.lima._prebuilt_peppylib_so_dir", return_value=so_dir):
        mock_shell.return_value = MagicMock(returncode=0)
        cargo_build_in_lima(
            limactl,
            "v0.1.0",
            "aarch64-unknown-linux-gnu",
            repo_root,
            target_dir=target_dir,
        )

    script = mock_shell.call_args[0][1]
    guest_target_dir = f"{GUEST_RUST_DIR}/target/{target_dir.name}"
    assert f"export CARGO_TARGET_DIR={guest_target_dir}" in script
    # The host-side target dir must not be cargo's output tree.
    assert f"CARGO_TARGET_DIR={target_dir}" not in script
    # The guest tree is deleted whether the build succeeds or fails.
    assert f"trap 'rm -rf {guest_target_dir}' EXIT" in script
    # Everything the host packaging step globs for is copied back.
    for pattern in GUEST_COPY_BACK_ARTIFACTS:
        assert pattern in script
    assert (
        f"host_release={target_dir}/aarch64-unknown-linux-gnu/release" in script
    )


def test_cargo_build_in_lima_sets_cross_linker_for_x86_64(tmp_path: Path) -> None:
    limactl = tmp_path / "limactl"
    repo_root = tmp_path / "repo"
    so_dir = _populate_so_dir(tmp_path)

    with patch("functions.lima._lima_shell") as mock_shell, \
         patch("functions.lima._prebuilt_peppylib_so_dir", return_value=so_dir):
        mock_shell.return_value = MagicMock(returncode=0)
        cargo_build_in_lima(
            limactl,
            "v0.1.0",
            "x86_64-unknown-linux-gnu",
            repo_root,
            target_dir=tmp_path / "target-x86",
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
            limactl,
            "v0.1.0",
            "aarch64-unknown-linux-gnu",
            repo_root,
            target_dir=tmp_path / "target-a64",
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
                limactl,
                "v0.1.0",
                "x86_64-unknown-linux-gnu",
                repo_root,
                target_dir=tmp_path / "target-x86",
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
                limactl,
                "v0.1.0",
                "x86_64-unknown-linux-gnu",
                repo_root,
                target_dir=tmp_path / "target-x86",
            )

    # The VM build must not start when the host bindings are absent.
    mock_shell.assert_not_called()
