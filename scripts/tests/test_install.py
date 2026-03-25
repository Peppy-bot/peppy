"""Tests for scripts/install.sh.

All tests run inside Lima VMs (Ubuntu 24.04, Fedora, Arch Linux) across
multiple architectures to replicate real-world install scenarios.  On
aarch64 hosts, x86_64 guest VMs are also spawned via QEMU emulation
(requires Lima >= 2.1.0) to catch cross-architecture bundling bugs.

On macOS hosts, a macOS aarch64 guest VM is included as well.
"""

from __future__ import annotations

import atexit
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Generator

import pytest

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
SCRIPTS_ROOT = REPO_ROOT / "scripts"
INSTALL_SCRIPT = SCRIPTS_ROOT / "install.sh"

LINUX_DISTROS = ["ubuntu", "fedora", "archlinux"]

# Arch Linux aarch64 has no official cloud image; the built-in
# template:archlinux uses a stale 2022 image from a slow GitHub mirror.
# We generate a template at test time that points to the latest release
# from SuperGregM/archlinux-arm-lima (automated monthly builds).
_ARCHLINUX_ARM_LIMA_API = (
    "https://api.github.com/repos/SuperGregM/archlinux-arm-lima/releases/latest"
)


def _host_arch() -> str:
    """Return the normalised host architecture."""
    machine = platform.machine()
    if machine in ("aarch64", "arm64"):
        return "aarch64"
    if machine in ("x86_64", "AMD64"):
        return "x86_64"
    raise RuntimeError(f"Unsupported host architecture: {machine}")


def _resolve_archlinux_template() -> str:
    """Fetch the latest Arch Linux aarch64 cloud image URL and return a
    path to a temporary Lima template YAML that references it.

    The custom template is only needed on aarch64 where the built-in
    template:archlinux ships a stale 2022 image.  On x86_64 the built-in
    template has a recent image and works out of the box.
    """
    if _host_arch() != "aarch64":
        return "template:archlinux"

    try:
        req = urllib.request.Request(
            _ARCHLINUX_ARM_LIMA_API,
            headers={"Accept": "application/vnd.github+json"},
        )
        with urllib.request.urlopen(req, timeout=15) as resp:
            data = json.loads(resp.read())

        # Find the dated qcow2.xz asset (skip the "latest" alias)
        image_url = None
        for asset in data.get("assets", []):
            name = asset["name"]
            if (
                name.endswith(".qcow2.xz")
                and "cloudimg" in name
                and "latest" not in name
            ):
                image_url = asset["browser_download_url"]
                break

        if not image_url:
            return "template:archlinux"

        yaml_content = (
            "# Auto-generated Arch Linux aarch64 template (latest release)\n"
            "images:\n"
            f'  - location: "{image_url}"\n'
            "    arch: aarch64\n"
        )

        # Write to a persistent temp file (Lima needs to read it later)
        fd, path = tempfile.mkstemp(suffix=".yaml", prefix="lima-archlinux-")
        os.write(fd, yaml_content.encode())
        os.close(fd)
        return path

    except (urllib.error.URLError, json.JSONDecodeError, KeyError, OSError):
        return "template:archlinux"


# Resolved once at module import so all tests share the same template.
# Cleaned up at process exit (not per-fixture) because multiple VM configs
# may reference the same temp file.
_ARCHLINUX_TEMPLATE = _resolve_archlinux_template()
if _ARCHLINUX_TEMPLATE.startswith("/"):
    atexit.register(
        lambda p=_ARCHLINUX_TEMPLATE: os.unlink(p) if os.path.isfile(p) else None
    )


# ---------------------------------------------------------------------------
# VMConfig — the central parameterisation unit
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class VMConfig:
    """Describes a Lima VM target for install testing."""

    os: str  # "linux" or "darwin"
    arch: str  # "aarch64" or "x86_64"
    distro: str | None  # "ubuntu", "fedora", "archlinux", or None (macOS)

    @property
    def target_triple(self) -> str:
        if self.os == "darwin":
            return "aarch64-apple-darwin"
        return f"{self.arch}-unknown-linux-gnu"

    @property
    def instance_name(self) -> str:
        # Keep short to stay under macOS UNIX_PATH_MAX=104 for ssh.sock paths.
        # Format: pti-{arch_short}-{distro_short_or_os}
        arch_short = "a64" if self.arch == "aarch64" else "x64"
        if self.distro:
            distro_map = {"ubuntu": "ubu", "fedora": "fed", "archlinux": "arch"}
            return f"pti-{arch_short}-{distro_map[self.distro]}"
        return f"pti-{arch_short}-{self.os[:3]}"

    @property
    def is_cross_arch(self) -> bool:
        """True if guest arch differs from host arch."""
        return self.arch != _host_arch()

    @property
    def template(self) -> str:
        if self.os == "darwin":
            return "template:macos"
        if self.distro == "archlinux":
            # The custom template is aarch64-only; x86_64 uses the built-in.
            return (
                _ARCHLINUX_TEMPLATE if self.arch == "aarch64" else "template:archlinux"
            )
        templates = {
            "ubuntu": "template:ubuntu-24.04",
            "fedora": "template:fedora",
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


def _build_vm_configs() -> list[VMConfig]:
    """Build the list of VM configs based on the host platform."""
    configs: list[VMConfig] = []
    host = _host_arch()
    cross = "x86_64" if host == "aarch64" else "aarch64"

    # Native-arch Linux VMs (all distros)
    for distro in LINUX_DISTROS:
        configs.append(VMConfig(os="linux", arch=host, distro=distro))

    # Cross-arch Linux VMs (all distros)
    for distro in LINUX_DISTROS:
        configs.append(VMConfig(os="linux", arch=cross, distro=distro))

    return configs


VM_CONFIGS = _build_vm_configs()


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _diagnostic(result: subprocess.CompletedProcess[str]) -> str:
    """Format stdout/stderr from a completed process for assertion messages."""
    return f"\n--- stdout ---\n{result.stdout}\n--- stderr ---\n{result.stderr}"


def _lima_env() -> dict[str, str]:
    """Environment with LIMA_HOME pointing to a test-specific directory.

    When running under pytest-xdist, each worker gets its own LIMA_HOME
    to avoid conflicts between parallel VM operations.
    """
    worker = os.environ.get("PYTEST_XDIST_WORKER", "gw0")
    lima_home = Path.home() / ".peppy" / f"lti-{worker}"
    lima_home.mkdir(parents=True, exist_ok=True)
    return {**os.environ, "LIMA_HOME": str(lima_home)}


def _lima_shell(
    script: str, *, instance: str, timeout: int = 120
) -> subprocess.CompletedProcess[str]:
    """Run a bash script inside the test Lima VM for the given instance."""
    return subprocess.run(
        [
            "limactl",
            "shell",
            "--workdir=/tmp",
            instance,
            "--",
            "bash",
            "-c",
            script,
        ],
        env=_lima_env(),
        capture_output=True,
        text=True,
        timeout=timeout,
    )


def _copy_to_lima(local_path: Path, guest_path: str, *, instance: str) -> None:
    """Copy a file from the host to the guest VM."""
    subprocess.run(
        ["limactl", "copy", str(local_path), f"{instance}:{guest_path}"],
        env=_lima_env(),
        check=True,
        timeout=30,
    )


def _guest_home(test_name: str) -> str:
    """Return a unique PEPPY_HOME path on the guest for the given test."""
    return f"/tmp/peppy-test-home/{test_name}"


def _find_release_archive(config: VMConfig) -> Path:
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


def _archive_guest_path(config: VMConfig) -> str:
    """Return the guest-side path for the release archive."""
    return f"/tmp/peppy-test/peppy-{config.target_triple}.tgz"


def _install_cmd(config: VMConfig, home: str, *, extra_env: str = "") -> str:
    """Build the install.sh invocation command for the guest."""
    env_parts = f"PEPPY_HOME={home} PEPPY_NO_SERVICE_INSTALL=1"
    if extra_env:
        env_parts = f"{env_parts} {extra_env}"
    return f"{env_parts} sh /tmp/peppy-test/install.sh {_archive_guest_path(config)}"


def _setup_lima_guest(config: VMConfig, *, test_name: str) -> str:
    """Copy install.sh and the release archive into the Lima guest.

    Returns the guest PEPPY_HOME path for this test.
    """
    archive_path = _find_release_archive(config)
    guest_home = _guest_home(test_name)
    instance = config.instance_name

    # Clean up all previous test homes to free disk space (real archives
    # are large and VMs have limited /tmp).
    _lima_shell(
        "rm -rf /tmp/peppy-test-home && mkdir -p /tmp/peppy-test",
        instance=instance,
    )
    _copy_to_lima(INSTALL_SCRIPT, "/tmp/peppy-test/install.sh", instance=instance)
    _copy_to_lima(archive_path, _archive_guest_path(config), instance=instance)
    return guest_home


# ---------------------------------------------------------------------------
# Build fixture — builds release archives once before any test runs
# ---------------------------------------------------------------------------


@pytest.fixture(scope="session", autouse=True)
def _build_release_archives() -> None:
    """Build all release archives from the current source tree.

    Runs once per test session before any install test executes.  The
    archives land in REPO_ROOT/dist/ where ``_find_release_archive``
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

    tag = "test"
    targets = get_targets_for_platform()
    _build_all_targets(tag, targets, REPO_ROOT)


# ---------------------------------------------------------------------------
# Lima VM fixture
# ---------------------------------------------------------------------------


@pytest.fixture(scope="module", params=VM_CONFIGS, ids=lambda c: c.pytest_id())
def lima_vm(request) -> Generator[VMConfig, None, None]:
    """Create (or reuse) an ephemeral Lima VM for install.sh testing.

    Parameterized across all VM configs (distro x arch combos).  Each VM
    is started once per config per test module and deleted on teardown.
    """
    yield from _lima_vm_lifecycle(request)


def _lima_vm_lifecycle(request):  # noqa: ANN001
    """Shared VM lifecycle used by both ``lima_vm`` and ``lima_linux_vm``."""
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

    env = _lima_env()

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
            "--disk=20",
            template,
        ]
        try:
            result = subprocess.run(
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
        if result.returncode != 0:
            pytest.fail(
                f"limactl start failed for {vm_label} "
                f"(exit {result.returncode}):\n{result.stderr}"
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

    # Note: dynamically-generated template files (e.g. archlinux aarch64 YAML)
    # are cleaned up at process exit via atexit, not here, because multiple
    # VM configs may share the same template file.


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


def test_install(lima_vm: VMConfig) -> None:
    """Run install.sh in the Lima VM and verify success."""
    config = lima_vm
    home = _setup_lima_guest(config, test_name=f"test_install_{config.pytest_id()}")

    result = _lima_shell(
        _install_cmd(config, home),
        instance=config.instance_name,
    )

    assert result.returncode == 0, (
        f"install.sh exited with {result.returncode} on {config.pytest_id()}"
        f"{_diagnostic(result)}"
    )
    assert "peppy installed to" in result.stdout, (
        f"Missing 'peppy installed to' in output on {config.pytest_id()}"
        f"{_diagnostic(result)}"
    )

    # Verify the installed binary is executable and responds to --help
    check = _lima_shell(
        f"test -x {home}/bin/peppy && {home}/bin/peppy --help",
        instance=config.instance_name,
    )
    assert check.returncode == 0, (
        f"peppy binary should be executable and respond to --help on "
        f"{config.pytest_id()}{_diagnostic(check)}"
    )


def test_no_root_install_happy_path(lima_vm: VMConfig) -> None:
    """PEPPY_NO_ROOT_INSTALL=1: install succeeds, setuid setup skipped."""
    config = lima_vm
    home = _setup_lima_guest(
        config, test_name=f"test_no_root_install_happy_path_{config.pytest_id()}"
    )

    result = _lima_shell(
        _install_cmd(config, home, extra_env="PEPPY_NO_ROOT_INSTALL=1"),
        instance=config.instance_name,
    )

    assert result.returncode == 0, (
        f"install.sh exited with {result.returncode} on {config.pytest_id()}"
        f"{_diagnostic(result)}"
    )
    assert "Skipped Apptainer setuid setup" in result.stdout, (
        f"Missing phase 2 skip message on {config.pytest_id()}{_diagnostic(result)}"
    )
    assert "peppy installed to" in result.stdout, (
        f"Missing 'peppy installed to' on {config.pytest_id()}{_diagnostic(result)}"
    )

    # Verify apptainer directory was extracted but starter-suid is NOT root-owned
    if config.os == "linux":
        check = _lima_shell(
            f"test -d {home}/bin/apptainer"
            f" && stat -c '%u' {home}/bin/apptainer/libexec/apptainer/bin/starter-suid",
            instance=config.instance_name,
        )
        assert check.returncode == 0, (
            f"apptainer dir should exist on {config.pytest_id()}"
        )
        owner_uid = check.stdout.strip()
        assert owner_uid != "0", (
            f"starter-suid should NOT be root-owned on {config.pytest_id()}, "
            f"but uid is {owner_uid}"
        )


def test_no_root_install_missing_dbus(lima_vm: VMConfig) -> None:
    """PEPPY_NO_ROOT_INSTALL=1 with D-Bus session bus unavailable: hard error."""
    config = lima_vm
    home = _setup_lima_guest(
        config, test_name=f"test_no_root_install_missing_dbus_{config.pytest_id()}"
    )

    # Point DBUS_SESSION_BUS_ADDRESS to a non-existent socket so dbus-send
    # --session fails to connect, simulating a system without a working D-Bus
    # user session.  Simply unsetting the variable is not enough because
    # dbus-send falls back to the well-known socket /run/user/UID/bus.
    result = _lima_shell(
        _install_cmd(
            config,
            home,
            extra_env=(
                "DBUS_SESSION_BUS_ADDRESS=unix:path=/dev/null/nonexistent "
                "PEPPY_NO_ROOT_INSTALL=1"
            ),
        ),
        instance=config.instance_name,
    )

    output = result.stdout + result.stderr

    assert result.returncode != 0, f"install.sh should have failed{_diagnostic(result)}"
    assert "D-Bus user session bus is not available" in output, (
        f"error should mention D-Bus user session bus{_diagnostic(result)}"
    )


def test_standard_install_sets_up_setuid(lima_vm: VMConfig) -> None:
    """Default install (no PEPPY_NO_ROOT_INSTALL): setuid is configured."""
    config = lima_vm
    assert config.os == "linux", "setuid test only applies to Linux VMs"

    home = _setup_lima_guest(
        config,
        test_name=f"test_standard_install_sets_up_setuid_{config.pytest_id()}",
    )

    result = _lima_shell(
        _install_cmd(config, home),
        instance=config.instance_name,
    )

    assert result.returncode == 0, (
        f"install.sh exited with {result.returncode} on {config.pytest_id()}"
        f"{_diagnostic(result)}"
    )
    assert "System dependencies configured successfully" in result.stdout, (
        f"Missing system dependencies success message on {config.pytest_id()}"
        f"{_diagnostic(result)}"
    )

    # Verify starter-suid is root-owned with setuid bit
    check = _lima_shell(
        f"stat -c '%u %a' {home}/bin/apptainer/libexec/apptainer/bin/starter-suid",
        instance=config.instance_name,
    )
    parts = check.stdout.strip().split()
    assert len(parts) == 2, (
        f"unexpected stat output on {config.pytest_id()}: {check.stdout}"
    )
    uid, mode = parts[0], parts[1]
    assert uid == "0", (
        f"starter-suid should be root-owned on {config.pytest_id()}, got uid {uid}"
    )
    assert mode == "4755", (
        f"starter-suid should have mode 4755 on {config.pytest_id()}, got {mode}"
    )


def test_reinstall_over_root_owned_files(lima_vm: VMConfig) -> None:
    """Reinstall succeeds even when previous install left root-owned Apptainer files."""
    config = lima_vm
    assert config.os == "linux", "reinstall test only applies to Linux VMs"

    home = _setup_lima_guest(
        config, test_name=f"test_reinstall_over_root_owned_{config.pytest_id()}"
    )

    # First install: creates root-owned apptainer files via setuid setup
    first = _lima_shell(
        _install_cmd(config, home, extra_env="PEPPY_FORCE_REINSTALL=1"),
        instance=config.instance_name,
    )
    assert first.returncode == 0, (
        f"first install failed on {config.pytest_id()}{_diagnostic(first)}"
    )

    # Verify root-owned files exist (confirms setuid setup ran)
    check = _lima_shell(
        f"stat -c '%u' {home}/bin/apptainer/etc/apptainer/apptainer.conf",
        instance=config.instance_name,
    )
    assert check.stdout.strip() == "0", (
        f"apptainer config should be root-owned after first install on "
        f"{config.pytest_id()}"
    )

    # Second install: must handle root-owned files without errors
    second = _lima_shell(
        _install_cmd(config, home, extra_env="PEPPY_FORCE_REINSTALL=1"),
        instance=config.instance_name,
    )
    assert second.returncode == 0, (
        f"reinstall failed on {config.pytest_id()}{_diagnostic(second)}"
    )
    assert "Permission denied" not in second.stderr, (
        f"reinstall should not produce permission errors on {config.pytest_id()}"
        f"{_diagnostic(second)}"
    )
    assert "peppy installed to" in second.stdout, (
        f"Missing 'peppy installed to' after reinstall on {config.pytest_id()}"
        f"{_diagnostic(second)}"
    )


def test_existing_install_warning(lima_vm: VMConfig) -> None:
    """When PEPPY_HOME exists but daemon is not running, show existing install warning."""
    config = lima_vm
    home = _setup_lima_guest(
        config, test_name=f"test_existing_install_warning_{config.pytest_id()}"
    )

    # Create PEPPY_HOME directory to simulate a previous install
    _lima_shell(f"mkdir -p {home}/bin", instance=config.instance_name)

    # Run without PEPPY_FORCE_REINSTALL — non-interactive should fail with
    # the "cannot prompt" error, proving the existing-install check triggered.
    result = _lima_shell(
        _install_cmd(config, home),
        instance=config.instance_name,
    )

    output = result.stdout + result.stderr

    assert "An existing installation was found" in output, (
        f"Missing existing-install warning on {config.pytest_id()}{_diagnostic(result)}"
    )


def test_unified_sudo_prompt_shows_all_items(lima_vm: VMConfig) -> None:
    """The pre-download sudo prompt lists Apptainer items on all distros."""
    config = lima_vm
    assert config.os == "linux", "sudo prompt test only applies to Linux VMs"

    home = _setup_lima_guest(
        config, test_name=f"test_unified_sudo_prompt_{config.pytest_id()}"
    )

    result = _lima_shell(
        _install_cmd(config, home),
        instance=config.instance_name,
    )

    output = result.stdout + result.stderr

    assert result.returncode == 0, (
        f"install.sh failed on {config.pytest_id()}{_diagnostic(result)}"
    )

    # The single prompt should contain both Apptainer items (present on all distros)
    assert "Set setuid permissions on Apptainer starter binary" in output, (
        f"Missing Apptainer setuid label in prompt on {config.pytest_id()}"
        f"{_diagnostic(result)}"
    )
    assert "Set root ownership on Apptainer configuration" in output, (
        f"Missing Apptainer config ownership label in prompt on {config.pytest_id()}"
        f"{_diagnostic(result)}"
    )

    # These labels must appear BEFORE the download (i.e. before "Extracting archive")
    prompt_pos = output.find("Set setuid permissions on Apptainer")
    extract_pos = output.find("Extracting archive")
    assert prompt_pos < extract_pos, (
        f"Apptainer labels should appear before extraction on {config.pytest_id()}"
        f"{_diagnostic(result)}"
    )


# ---------------------------------------------------------------------------
# Architecture validation tests
# ---------------------------------------------------------------------------


@pytest.mark.cross_arch
def test_binary_architecture(lima_vm: VMConfig) -> None:
    """Verify installed binaries match the guest architecture.

    Checks both the peppy binary and the apptainer binary via the ``file``
    command to ensure ELF architecture matches what the guest expects.
    This catches cross-architecture bundling bugs like the v0.5.6 issue
    where an aarch64 Apptainer binary was shipped in the x86_64 release.
    """
    config = lima_vm
    assert config.os == "linux", "binary architecture test only applies to Linux VMs"

    home = _setup_lima_guest(config, test_name=f"test_binary_arch_{config.pytest_id()}")

    result = _lima_shell(
        _install_cmd(config, home),
        instance=config.instance_name,
    )
    assert result.returncode == 0, (
        f"install failed on {config.pytest_id()}{_diagnostic(result)}"
    )

    expected_arch = {"x86_64": "x86-64", "aarch64": "aarch64"}[config.arch]

    # Check peppy binary
    check = _lima_shell(f"file {home}/bin/peppy", instance=config.instance_name)
    assert expected_arch in check.stdout, (
        f"peppy binary arch mismatch on {config.pytest_id()}: "
        f"expected '{expected_arch}'{_diagnostic(check)}"
    )

    # Check apptainer binary
    check = _lima_shell(
        f"file {home}/bin/apptainer/bin/apptainer",
        instance=config.instance_name,
    )
    assert expected_arch in check.stdout, (
        f"apptainer binary arch mismatch on {config.pytest_id()}: "
        f"expected '{expected_arch}'{_diagnostic(check)}"
    )


@pytest.mark.cross_arch
def test_peppylib_so_architecture(lima_vm: VMConfig) -> None:
    """Verify peppylib .abi3.so matches the guest architecture.

    Installs peppy, starts the daemon, runs ``peppy node add .`` on a
    minimal Python project, then checks the extracted .so architecture
    in ~/.peppy.
    """
    config = lima_vm
    assert config.os == "linux", "peppylib .so test only applies to Linux VMs"

    home = _setup_lima_guest(config, test_name=f"test_peppylib_so_{config.pytest_id()}")

    # Kill any leftover daemon from a previous run
    _lima_shell(
        f"pkill -f 'peppy service serve' 2>/dev/null; rm -rf {home}; true",
        instance=config.instance_name,
    )

    # Install peppy (force reinstall in case PEPPY_HOME existed)
    install = _lima_shell(
        _install_cmd(config, home, extra_env="PEPPY_FORCE_REINSTALL=1"),
        instance=config.instance_name,
    )
    assert install.returncode == 0, (
        f"install failed on {config.pytest_id()}{_diagnostic(install)}"
    )

    timeout = 3600 if config.is_cross_arch else 600

    # Start daemon, init a Python node, add it, then check .so arch.
    # peppy node init creates a directory with peppy.json5; peppy node add
    # registers it with the daemon and triggers peppylib extraction.
    # The add_cmd (uv sync) may fail if uv isn't installed in the VM, but
    # the peppylib .so is extracted before add_cmd runs, so we check for
    # its presence regardless of the add_cmd outcome.
    #
    # The script captures the `file` output before killing the daemon to
    # avoid SIGTERM-related exit codes (143).
    script = f"""\
set -eu
export PATH="{home}/bin:$PATH"
export PEPPY_HOME={home}

# Start daemon in background
peppy service serve &
DAEMON_PID=$!
sleep 5

# Create a Python node (default toolchain is uv = Python)
cd /tmp
rm -rf test-node
peppy node init test-node

# Add the node to the daemon (triggers peppylib .so extraction).
# This may fail if uv is not installed — that's OK, we only need the
# .so extraction which happens before add_cmd.
peppy node add /tmp/test-node || true

# Find .abi3.so — it may be in the node working dir, the daemon's data
# directory (~/.peppy), or the custom PEPPY_HOME.
SO_FILE=$(find /tmp/test-node {home} $HOME/.peppy -name '*.abi3*.so' -type f 2>/dev/null | head -1)

# Kill daemon before checking results to avoid SIGTERM exit codes
kill $DAEMON_PID 2>/dev/null; wait $DAEMON_PID 2>/dev/null || true

if [ -z "$SO_FILE" ]; then
    echo "ERROR: No .abi3.so found in /tmp/test-node, {home}, or ~/.peppy"
    exit 1
fi
echo "FOUND_SO=$SO_FILE"
file "$SO_FILE"
"""
    result = _lima_shell(script, instance=config.instance_name, timeout=timeout)

    expected_arch = {"x86_64": "x86-64", "aarch64": "aarch64"}[config.arch]
    assert result.returncode == 0, (
        f"peppylib .so test failed on {config.pytest_id()}{_diagnostic(result)}"
    )
    assert expected_arch in result.stdout, (
        f".abi3.so arch mismatch on {config.pytest_id()}: "
        f"expected '{expected_arch}'{_diagnostic(result)}"
    )
