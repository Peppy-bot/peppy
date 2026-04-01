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
import sys
import tempfile
import urllib.request
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Generator

import pytest

from .lima_helpers import (
    VMConfig,
    diagnostic,
    host_arch,
    install_cmd,
    lima_shell,
    lima_vm_lifecycle,
    setup_lima_guest,
)

LINUX_DISTROS = ["ubuntu", "fedora", "archlinux"]

# Arch Linux aarch64 has no official cloud image; the built-in
# template:archlinux uses a stale 2022 image from a slow GitHub mirror.
# We generate a template at test time that points to the latest release
# from SuperGregM/archlinux-arm-lima (automated monthly builds).
_ARCHLINUX_ARM_LIMA_API = (
    "https://api.github.com/repos/SuperGregM/archlinux-arm-lima/releases/latest"
)


def _resolve_archlinux_template() -> str:
    """Fetch the latest Arch Linux aarch64 cloud image URL and return a
    path to a temporary Lima template YAML that references it.

    The custom template is only needed on aarch64 where the built-in
    template:archlinux ships a stale 2022 image.  On x86_64 the built-in
    template has a recent image and works out of the box.
    """
    if host_arch() != "aarch64":
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
# VM config construction
# ---------------------------------------------------------------------------


def _build_vm_configs() -> list[VMConfig]:
    """Build the list of VM configs based on the host platform.

    On macOS all release triples are built, so both native and cross-arch
    Linux VMs are included.  On Linux only the native triple is produced,
    so cross-arch VM configs are omitted (no archive to test with).

    The archlinux aarch64 template is resolved dynamically (see above),
    so archlinux configs use ``template_override`` to point at the custom
    template YAML.
    """
    configs: list[VMConfig] = []
    host = host_arch()
    cross = "x86_64" if host == "aarch64" else "aarch64"

    def _archlinux_template(arch: str) -> str:
        return _ARCHLINUX_TEMPLATE if arch == "aarch64" else "template:archlinux"

    # Native-arch Linux VMs (all distros)
    for distro in LINUX_DISTROS:
        override = _archlinux_template(host) if distro == "archlinux" else None
        configs.append(
            VMConfig(os="linux", arch=host, distro=distro, template_override=override)
        )

    # Cross-arch Linux VMs — only when all triples are built (macOS).
    if sys.platform == "darwin":
        for distro in LINUX_DISTROS:
            override = _archlinux_template(cross) if distro == "archlinux" else None
            configs.append(
                VMConfig(
                    os="linux", arch=cross, distro=distro, template_override=override
                )
            )

    return configs


VM_CONFIGS = _build_vm_configs()


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture(scope="module", params=VM_CONFIGS, ids=lambda c: c.pytest_id())
def lima_vm(request, _build_release_archives) -> Generator[VMConfig, None, None]:
    """Create (or reuse) an ephemeral Lima VM for install.sh testing.

    Parameterized across all VM configs (distro x arch combos).  Each VM
    is started once per config per test module and deleted on teardown.
    """
    yield from lima_vm_lifecycle(request)

    # Note: dynamically-generated template files (e.g. archlinux aarch64 YAML)
    # are cleaned up at process exit via atexit, not here, because multiple
    # VM configs may share the same template file.


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


def test_install(lima_vm: VMConfig) -> None:
    """Run install.sh in the Lima VM and verify success."""
    config = lima_vm
    home = setup_lima_guest(config, test_name=f"test_install_{config.pytest_id()}")

    result = lima_shell(
        install_cmd(config, home),
        instance=config.instance_name,
    )

    assert result.returncode == 0, (
        f"install.sh exited with {result.returncode} on {config.pytest_id()}"
        f"{diagnostic(result)}"
    )
    assert "peppy installed to" in result.stdout, (
        f"Missing 'peppy installed to' in output on {config.pytest_id()}"
        f"{diagnostic(result)}"
    )

    # Verify the installed binary is executable and responds to --help
    check = lima_shell(
        f"test -x {home}/bin/peppy && {home}/bin/peppy --help",
        instance=config.instance_name,
    )
    assert check.returncode == 0, (
        f"peppy binary should be executable and respond to --help on "
        f"{config.pytest_id()}{diagnostic(check)}"
    )


def test_no_root_install_happy_path(lima_vm: VMConfig) -> None:
    """PEPPY_NO_ROOT_INSTALL=1: install succeeds, container setup skipped."""
    config = lima_vm
    home = setup_lima_guest(
        config, test_name=f"test_no_root_install_happy_path_{config.pytest_id()}"
    )

    result = lima_shell(
        install_cmd(config, home, extra_env="PEPPY_NO_ROOT_INSTALL=1"),
        instance=config.instance_name,
    )

    assert result.returncode == 0, (
        f"install.sh exited with {result.returncode} on {config.pytest_id()}"
        f"{diagnostic(result)}"
    )
    assert "Skipped Apptainer setup" in result.stdout, (
        f"Missing setup skip message on {config.pytest_id()}{diagnostic(result)}"
    )
    assert "peppy installed to" in result.stdout, (
        f"Missing 'peppy installed to' on {config.pytest_id()}{diagnostic(result)}"
    )

    # Verify apptainer directory was extracted
    if config.os == "linux":
        check = lima_shell(
            f"test -d {home}/bin/apptainer"
            f" && test -f {home}/bin/apptainer/bin/apptainer",
            instance=config.instance_name,
        )
        assert check.returncode == 0, (
            f"apptainer dir and binary should exist on {config.pytest_id()}"
        )


def test_no_root_install_missing_dbus(lima_vm: VMConfig) -> None:
    """PEPPY_NO_ROOT_INSTALL=1 with D-Bus session bus unavailable: hard error."""
    config = lima_vm
    home = setup_lima_guest(
        config, test_name=f"test_no_root_install_missing_dbus_{config.pytest_id()}"
    )

    # Point DBUS_SESSION_BUS_ADDRESS to a non-existent socket so dbus-send
    # --session fails to connect, simulating a system without a working D-Bus
    # user session.  Simply unsetting the variable is not enough because
    # dbus-send falls back to the well-known socket /run/user/UID/bus.
    result = lima_shell(
        install_cmd(
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

    assert result.returncode != 0, f"install.sh should have failed{diagnostic(result)}"
    assert "D-Bus user session bus is not available" in output, (
        f"error should mention D-Bus user session bus{diagnostic(result)}"
    )


def test_standard_install_container_setup(lima_vm: VMConfig) -> None:
    """Default install (no PEPPY_NO_ROOT_INSTALL): container setup runs."""
    config = lima_vm
    assert config.os == "linux", "container setup test only applies to Linux VMs"

    home = setup_lima_guest(
        config,
        test_name=f"test_standard_install_container_setup_{config.pytest_id()}",
    )

    result = lima_shell(
        install_cmd(config, home),
        instance=config.instance_name,
    )

    assert result.returncode == 0, (
        f"install.sh exited with {result.returncode} on {config.pytest_id()}"
        f"{diagnostic(result)}"
    )

    # Verify apptainer starter binary exists (no starter-suid in --without-suid builds)
    check = lima_shell(
        f"test -f {home}/bin/apptainer/libexec/apptainer/bin/starter",
        instance=config.instance_name,
    )
    assert check.returncode == 0, (
        f"apptainer starter binary should exist on {config.pytest_id()}"
    )


def test_reinstall_over_root_owned_files(lima_vm: VMConfig) -> None:
    """Reinstall succeeds even when previous install left files behind."""
    config = lima_vm
    assert config.os == "linux", "reinstall test only applies to Linux VMs"

    home = setup_lima_guest(
        config, test_name=f"test_reinstall_over_root_owned_{config.pytest_id()}"
    )

    # First install
    first = lima_shell(
        install_cmd(config, home, extra_env="PEPPY_FORCE_REINSTALL=1"),
        instance=config.instance_name,
    )
    assert first.returncode == 0, (
        f"first install failed on {config.pytest_id()}{diagnostic(first)}"
    )

    # Verify apptainer files exist
    check = lima_shell(
        f"test -f {home}/bin/apptainer/etc/apptainer/apptainer.conf",
        instance=config.instance_name,
    )
    assert check.returncode == 0, (
        f"apptainer config should exist after first install on "
        f"{config.pytest_id()}"
    )

    # Second install: must handle root-owned files without errors
    second = lima_shell(
        install_cmd(config, home, extra_env="PEPPY_FORCE_REINSTALL=1"),
        instance=config.instance_name,
    )
    assert second.returncode == 0, (
        f"reinstall failed on {config.pytest_id()}{diagnostic(second)}"
    )
    assert "Permission denied" not in second.stderr, (
        f"reinstall should not produce permission errors on {config.pytest_id()}"
        f"{diagnostic(second)}"
    )
    assert "peppy installed to" in second.stdout, (
        f"Missing 'peppy installed to' after reinstall on {config.pytest_id()}"
        f"{diagnostic(second)}"
    )


def test_existing_install_warning(lima_vm: VMConfig) -> None:
    """When PEPPY_HOME exists but daemon is not running, show existing install warning."""
    config = lima_vm
    home = setup_lima_guest(
        config, test_name=f"test_existing_install_warning_{config.pytest_id()}"
    )

    # Create PEPPY_HOME directory to simulate a previous install
    lima_shell(f"mkdir -p {home}/bin", instance=config.instance_name)

    # Run without PEPPY_FORCE_REINSTALL — non-interactive should fail with
    # the "cannot prompt" error, proving the existing-install check triggered.
    result = lima_shell(
        install_cmd(config, home),
        instance=config.instance_name,
    )

    output = result.stdout + result.stderr

    assert "An existing installation was found" in output, (
        f"Missing existing-install warning on {config.pytest_id()}{diagnostic(result)}"
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

    home = setup_lima_guest(config, test_name=f"test_binary_arch_{config.pytest_id()}")

    result = lima_shell(
        install_cmd(config, home),
        instance=config.instance_name,
    )
    assert result.returncode == 0, (
        f"install failed on {config.pytest_id()}{diagnostic(result)}"
    )

    expected_arch = {"x86_64": "x86-64", "aarch64": "aarch64"}[config.arch]

    # Check peppy binary
    check = lima_shell(f"file {home}/bin/peppy", instance=config.instance_name)
    assert expected_arch in check.stdout, (
        f"peppy binary arch mismatch on {config.pytest_id()}: "
        f"expected '{expected_arch}'{diagnostic(check)}"
    )

    # Check apptainer binary
    check = lima_shell(
        f"file {home}/bin/apptainer/bin/apptainer",
        instance=config.instance_name,
    )
    assert expected_arch in check.stdout, (
        f"apptainer binary arch mismatch on {config.pytest_id()}: "
        f"expected '{expected_arch}'{diagnostic(check)}"
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

    home = setup_lima_guest(config, test_name=f"test_peppylib_so_{config.pytest_id()}")

    # Kill any leftover daemon from a previous run
    lima_shell(
        f"pkill -f 'peppy service serve' 2>/dev/null; rm -rf {home}; true",
        instance=config.instance_name,
    )

    # Install peppy (force reinstall in case PEPPY_HOME existed)
    install = lima_shell(
        install_cmd(config, home, extra_env="PEPPY_FORCE_REINSTALL=1"),
        instance=config.instance_name,
    )
    assert install.returncode == 0, (
        f"install failed on {config.pytest_id()}{diagnostic(install)}"
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
cd /var/tmp
rm -rf test-node
peppy node init test-node

# Add the node to the daemon (triggers peppylib .so extraction).
# This may fail if uv is not installed — that's OK, we only need the
# .so extraction which happens before add_cmd.
peppy node add /var/tmp/test-node || true

# Find .abi3.so — it may be in the node working dir, the daemon's data
# directory (~/.peppy), or the custom PEPPY_HOME.
SO_FILE=$(find /var/tmp/test-node {home} $HOME/.peppy -name '*.abi3*.so' -type f 2>/dev/null | head -1)

# Kill daemon before checking results to avoid SIGTERM exit codes
kill $DAEMON_PID 2>/dev/null; wait $DAEMON_PID 2>/dev/null || true

if [ -z "$SO_FILE" ]; then
    echo "ERROR: No .abi3.so found in /var/tmp/test-node, {home}, or ~/.peppy"
    exit 1
fi
echo "FOUND_SO=$SO_FILE"
file "$SO_FILE"
"""
    result = lima_shell(script, instance=config.instance_name, timeout=timeout)

    expected_arch = {"x86_64": "x86-64", "aarch64": "aarch64"}[config.arch]
    assert result.returncode == 0, (
        f"peppylib .so test failed on {config.pytest_id()}{diagnostic(result)}"
    )
    assert expected_arch in result.stdout, (
        f".abi3.so arch mismatch on {config.pytest_id()}: "
        f"expected '{expected_arch}'{diagnostic(result)}"
    )
