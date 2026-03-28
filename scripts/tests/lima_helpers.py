"""Shared Lima VM helpers for install and container build tests."""

from __future__ import annotations

import logging
import os
import platform
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Generator

import pytest

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
SCRIPTS_ROOT = REPO_ROOT / "scripts"
INSTALL_SCRIPT = SCRIPTS_ROOT / "install.sh"

logger = logging.getLogger(__name__)

_SSH_MAX_RETRIES = 3
_SSH_RETRY_DELAY = 5  # seconds


# ---------------------------------------------------------------------------
# VMConfig
# ---------------------------------------------------------------------------


def host_arch() -> str:
    """Return the normalised host architecture."""
    machine = platform.machine()
    if machine in ("aarch64", "arm64"):
        return "aarch64"
    if machine in ("x86_64", "AMD64"):
        return "x86_64"
    raise RuntimeError(f"Unsupported host architecture: {machine}")


def get_native_linux_targets() -> list[str]:
    """Return only the native-arch Linux target triple."""
    return [f"{host_arch()}-unknown-linux-gnu"]


@dataclass(frozen=True)
class VMConfig:
    """Describes a Lima VM target for install testing."""

    os: str  # "linux" or "darwin"
    arch: str  # "aarch64" or "x86_64"
    distro: str | None  # "ubuntu", "fedora", "archlinux", or None (macOS)
    instance_prefix: str = "pti"  # prefix for instance names
    template_override: str | None = None  # override the default template

    @property
    def target_triple(self) -> str:
        if self.os == "darwin":
            return "aarch64-apple-darwin"
        return f"{self.arch}-unknown-linux-gnu"

    @property
    def instance_name(self) -> str:
        # Keep short to stay under macOS UNIX_PATH_MAX=104 for ssh.sock paths.
        arch_short = "a64" if self.arch == "aarch64" else "x64"
        if self.distro:
            distro_map = {"ubuntu": "ubu", "fedora": "fed", "archlinux": "arch"}
            return f"{self.instance_prefix}-{arch_short}-{distro_map[self.distro]}"
        return f"{self.instance_prefix}-{arch_short}-{self.os[:3]}"

    @property
    def is_cross_arch(self) -> bool:
        """True if guest arch differs from host arch."""
        return self.arch != host_arch()

    @property
    def template(self) -> str:
        if self.template_override:
            return self.template_override
        if self.os == "darwin":
            return "template:macos"
        templates = {
            "ubuntu": "template:ubuntu-24.04",
            "fedora": "template:fedora",
            "archlinux": "template:archlinux",
        }
        return templates[self.distro]

    @property
    def lima_arch_flag(self) -> list[str]:
        """Return --arch flag if cross-arch, else empty."""
        if self.is_cross_arch:
            return [f"--arch={self.arch}"]
        return []

    def pytest_id(self) -> str:
        if self.distro:
            return f"{self.os}-{self.arch}-{self.distro}"
        return f"{self.os}-{self.arch}"


# ---------------------------------------------------------------------------
# Lima shell helpers
# ---------------------------------------------------------------------------


def diagnostic(result: subprocess.CompletedProcess[str]) -> str:
    """Format stdout/stderr from a completed process for assertion messages."""
    return f"\n--- stdout ---\n{result.stdout}\n--- stderr ---\n{result.stderr}"


def lima_env() -> dict[str, str]:
    """Environment with LIMA_HOME pointing to a test-specific directory.

    When running under pytest-xdist, each worker gets its own LIMA_HOME
    to avoid conflicts between parallel VM operations.
    """
    worker = os.environ.get("PYTEST_XDIST_WORKER", "gw0")
    lima_home = Path.home() / ".peppy" / f"lti-{worker}"
    lima_home.mkdir(parents=True, exist_ok=True)
    return {**os.environ, "LIMA_HOME": str(lima_home)}


def is_ssh_connection_error(result: subprocess.CompletedProcess[str]) -> bool:
    """Return True if the process failed due to a transient SSH/SCP error."""
    if result.returncode == 0:
        return False
    stderr = (result.stderr or "").lower()
    return result.returncode == 255 or "connection closed" in stderr


def lima_shell(
    script: str, *, instance: str, timeout: int = 120, retries: int = _SSH_MAX_RETRIES
) -> subprocess.CompletedProcess[str]:
    """Run a bash script inside the test Lima VM for the given instance.

    Retries on transient SSH connection errors (common with cross-arch QEMU VMs).
    """
    cmd = [
        "limactl",
        "shell",
        "--workdir=/tmp",
        instance,
        "--",
        "bash",
        "-c",
        script,
    ]
    env = lima_env()
    for attempt in range(1, retries + 1):
        result = subprocess.run(
            cmd, env=env, capture_output=True, text=True, timeout=timeout
        )
        if not is_ssh_connection_error(result) or attempt == retries:
            return result
        logger.warning(
            "SSH connection error (attempt %d/%d) for %s, retrying in %ds...",
            attempt,
            retries,
            instance,
            _SSH_RETRY_DELAY,
        )
        time.sleep(_SSH_RETRY_DELAY)
    return result  # unreachable, but satisfies type checkers


def copy_to_lima(
    local_path: Path, guest_path: str, *, instance: str, retries: int = _SSH_MAX_RETRIES
) -> None:
    """Copy a file from the host to the guest VM.

    Retries on transient SSH connection errors (common with cross-arch QEMU VMs).
    """
    cmd = ["limactl", "copy", str(local_path), f"{instance}:{guest_path}"]
    env = lima_env()
    for attempt in range(1, retries + 1):
        result = subprocess.run(
            cmd, env=env, capture_output=True, text=True, timeout=30
        )
        if result.returncode == 0:
            return
        if not is_ssh_connection_error(result) or attempt == retries:
            raise subprocess.CalledProcessError(
                result.returncode, cmd, output=result.stdout, stderr=result.stderr
            )
        logger.warning(
            "SCP connection error (attempt %d/%d) for %s, retrying in %ds...",
            attempt,
            retries,
            instance,
            _SSH_RETRY_DELAY,
        )
        time.sleep(_SSH_RETRY_DELAY)


# ---------------------------------------------------------------------------
# Guest path helpers
# ---------------------------------------------------------------------------


def guest_home(test_name: str) -> str:
    """Return a unique PEPPY_HOME path on the guest for the given test."""
    return f"/var/tmp/peppy-test-home/{test_name}"


def find_release_archive(config: VMConfig) -> Path:
    """Find the pre-built release archive for the given VMConfig.

    Looks in PEPPY_TEST_ARCHIVE_DIR (env var), then falls back to
    REPO_ROOT/dist/.  Raises pytest.fail() if the archive is not found.
    """
    triple = config.target_triple
    archive_name = f"peppy-{triple}.tgz"

    search_dirs: list[Path] = []
    env_dir = os.environ.get("PEPPY_TEST_ARCHIVE_DIR")
    if env_dir:
        search_dirs.append(Path(env_dir))
    search_dirs.append(REPO_ROOT / "dist")

    for d in search_dirs:
        path = d / archive_name
        if path.is_file():
            return path

    pytest.fail(
        f"Release archive {archive_name} not found "
        f"(searched: {', '.join(str(d) for d in search_dirs)})"
    )


def archive_guest_path(config: VMConfig) -> str:
    """Return the guest-side path for the release archive."""
    return f"/var/tmp/peppy-test/peppy-{config.target_triple}.tgz"


def install_cmd(config: VMConfig, home: str, *, extra_env: str = "") -> str:
    """Build the install.sh invocation command for the guest."""
    env_parts = f"PEPPY_HOME={home} PEPPY_NO_SERVICE_INSTALL=1"
    if extra_env:
        env_parts = f"{env_parts} {extra_env}"
    return f"{env_parts} sh /var/tmp/peppy-test/install.sh {archive_guest_path(config)}"


def setup_lima_guest(config: VMConfig, *, test_name: str) -> str:
    """Copy install.sh and the release archive into the Lima guest.

    Returns the guest PEPPY_HOME path for this test.
    """
    archive_path = find_release_archive(config)
    home = guest_home(test_name)
    instance = config.instance_name

    # Use /var/tmp (disk-backed) instead of /tmp to avoid tmpfs size
    # limits on Fedora and Arch Linux where /tmp is RAM-backed.
    lima_shell(
        "rm -rf /var/tmp/peppy-test-home && mkdir -p /var/tmp/peppy-test",
        instance=instance,
    )
    copy_to_lima(INSTALL_SCRIPT, "/var/tmp/peppy-test/install.sh", instance=instance)
    copy_to_lima(archive_path, archive_guest_path(config), instance=instance)
    return home


# ---------------------------------------------------------------------------
# Lima VM lifecycle
# ---------------------------------------------------------------------------


def lima_vm_lifecycle(request: pytest.FixtureRequest) -> Generator[VMConfig, None, None]:
    """Shared VM lifecycle: create, yield, teardown."""
    config: VMConfig = request.param
    instance = config.instance_name
    template = config.template

    if sys.platform == "linux" and shutil.which("qemu-img") is None:
        pytest.fail(
            "QEMU is required to run Lima VMs on Linux. "
            "Install it via: sudo apt install qemu-utils qemu-system"
        )

    if (
        sys.platform == "linux"
        and os.path.exists("/dev/kvm")
        and not os.access("/dev/kvm", os.R_OK | os.W_OK)
    ):
        pytest.fail(
            "KVM is not accessible (permission denied on /dev/kvm). "
            "Add your user to the kvm group: sudo usermod -aG kvm $(whoami) && newgrp kvm"
        )

    env = lima_env()

    result = subprocess.run(
        ["limactl", "list", "--format", "{{.Status}}", instance],
        env=env,
        capture_output=True,
        text=True,
    )
    status = result.stdout.strip()

    # macOS guests download a ~14GB IPSW; cross-arch VMs use slow QEMU emulation.
    if config.os == "darwin":
        vm_start_timeout = 3600
    elif config.is_cross_arch:
        vm_start_timeout = 1800
    else:
        vm_start_timeout = 900
    vm_label = config.pytest_id()

    def _start_lima_vm(extra_args: list[str] | None = None) -> None:
        cmd = [
            "limactl",
            "start",
            *(extra_args or []),
            *config.lima_arch_flag,
            f"--name={instance}",
            "--tty=false",
            "--mount-writable",
            "--containerd=none",
            "--disk=20",
            template,
        ]
        try:
            start_result = subprocess.run(
                cmd, env=env, capture_output=True, text=True, timeout=vm_start_timeout
            )
        except subprocess.TimeoutExpired:
            subprocess.run(
                ["limactl", "delete", "--force", instance],
                env=env,
                capture_output=True,
                timeout=60,
            )
            pytest.fail(
                f"Lima VM start for {vm_label} timed out after {vm_start_timeout}s "
                f"(cloud image download may be slow)"
            )
        if start_result.returncode != 0:
            pytest.fail(
                f"limactl start failed for {vm_label} "
                f"(exit {start_result.returncode}):\n{start_result.stderr}"
            )

    if not status:
        _start_lima_vm()
    elif status == "Stopped":
        try:
            restart = subprocess.run(
                ["limactl", "start", instance],
                env=env,
                capture_output=True,
                timeout=vm_start_timeout,
            )
        except subprocess.TimeoutExpired:
            subprocess.run(
                ["limactl", "delete", "--force", instance],
                env=env,
                capture_output=True,
                timeout=60,
            )
            pytest.fail(
                f"Lima VM restart for {vm_label} timed out after {vm_start_timeout}s"
            )
        if restart.returncode != 0:
            subprocess.run(
                ["limactl", "delete", "--force", instance],
                env=env,
                capture_output=True,
                timeout=60,
            )
            _start_lima_vm()

    yield config

    subprocess.run(
        ["limactl", "stop", instance],
        env=env,
        capture_output=True,
        timeout=60,
    )
    subprocess.run(
        ["limactl", "delete", instance],
        env=env,
        capture_output=True,
        timeout=60,
    )


# ---------------------------------------------------------------------------
# Build fixture helper
# ---------------------------------------------------------------------------


def build_release_archives(targets: list[str] | None = None) -> None:
    """Build release archives from the current source tree.

    When *targets* is ``None`` (the default), all targets for the current
    platform are built.  Pass an explicit list to restrict which triples
    are built — this avoids spawning cross-arch QEMU VMs when only
    native-arch archives are needed.

    Runs once per test session before any install test executes.  The
    archives land in REPO_ROOT/dist/ where ``find_release_archive``
    picks them up.

    Cleans the containers crate build cache first to ensure build.rs
    changes (e.g. architecture detection fixes) take effect.
    """
    from functions.build_release import _build_all_targets
    from functions.cli import get_targets_for_platform

    # Force rebuild of the containers crate so build.rs re-runs with the
    # latest arch detection logic.  Without this, stale build artifacts
    # may bundle the wrong architecture binaries.
    subprocess.run(
        ["cargo", "clean", "-p", "containers", "--release"],
        cwd=REPO_ROOT,
        capture_output=True,
    )

    # Clear apptainer source caches to force a full source build, ensuring
    # that build.rs source-build logic (e.g. VERSION file creation) is
    # always exercised rather than masked by stale cached binaries.
    peppy_tmp = Path.home() / ".peppy" / "tmp"
    if peppy_tmp.exists():
        for entry in peppy_tmp.iterdir():
            if entry.name.startswith("apptainer-") and entry.is_dir():
                shutil.rmtree(entry, ignore_errors=True)

    tag = "test"
    if targets is None:
        targets = get_targets_for_platform()
    _build_all_targets(tag, targets, REPO_ROOT)
