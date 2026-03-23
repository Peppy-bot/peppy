"""Tests for scripts/install.sh.

All tests run inside Lima VMs (Ubuntu 24.04, Fedora, Arch Linux) as a
non-root sudo user to replicate real-world install scenarios across
different Linux distributions.
"""

from __future__ import annotations

import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
INSTALL_SCRIPT = REPO_ROOT / "scripts" / "install.sh"

DISTROS = ["ubuntu", "fedora", "archlinux"]

# Ubuntu and Fedora use Lima's built-in templates which are updated with
# each Lima release.  Arch Linux aarch64 has no official cloud image; the
# built-in template:archlinux uses a stale 2022 image from a slow GitHub
# mirror.  We generate a template at test time that points to the latest
# release from SuperGregM/archlinux-arm-lima (automated monthly builds).
_ARCHLINUX_ARM_LIMA_API = (
    "https://api.github.com/repos/SuperGregM/archlinux-arm-lima/releases/latest"
)


def _resolve_archlinux_template() -> str:
    """Fetch the latest Arch Linux aarch64 cloud image URL and return a
    path to a temporary Lima template YAML that references it.

    Falls back to the built-in template:archlinux if the API call fails.
    """
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


def _get_templates() -> dict[str, str]:
    """Return the Lima template for each distro, resolving dynamic ones."""
    return {
        "ubuntu": "template:ubuntu-24.04",
        "fedora": "template:fedora",
        "archlinux": _resolve_archlinux_template(),
    }


# Resolved once at module import so all tests share the same templates.
TEMPLATES = _get_templates()


def _instance_name(distro: str) -> str:
    """Return the Lima instance name for the given distro."""
    return f"peppy-test-install-{distro}"


def _create_fake_archive(directory: Path, *, with_apptainer: bool = False) -> Path:
    """Create a minimal .tgz archive that satisfies install.sh.

    The archive mirrors the layout produced by ``package_release()``:
    ``./bin/peppy`` and ``./bin/zenohd`` as executable shell stubs.

    When *with_apptainer* is True the archive also contains stub Apptainer
    files so that install.sh phase 2 (setuid setup) has something to operate on.
    """
    archive_path = directory / "peppy-fake.tgz"

    peppy_stub = b"#!/bin/sh\necho peppy-stub\n"
    zenohd_stub = b"#!/bin/sh\necho zenohd-stub\n"

    entries: list[tuple[str, bytes, int]] = [
        ("./bin/peppy", peppy_stub, 0o755),
        ("./bin/zenohd", zenohd_stub, 0o755),
    ]

    if with_apptainer:
        apptainer_stub = b"#!/bin/sh\necho apptainer-stub\n"
        entries += [
            ("./bin/apptainer/bin/apptainer", apptainer_stub, 0o755),
            ("./bin/apptainer/libexec/apptainer/bin/starter-suid", b"fake-suid", 0o755),
            ("./bin/apptainer/etc/apptainer/apptainer.conf", b"# config\n", 0o644),
        ]

    with tarfile.open(archive_path, "w:gz") as tar:
        for name, content, mode in entries:
            info = tarfile.TarInfo(name=name)
            info.size = len(content)
            info.mode = mode
            tar.addfile(info, io.BytesIO(content))

    return archive_path


# ---------------------------------------------------------------------------
# Lima VM helpers
# ---------------------------------------------------------------------------


def _lima_env() -> dict[str, str]:
    """Environment with LIMA_HOME pointing to a test-specific directory."""
    lima_home = Path.home() / ".peppy" / "lima-test-install"
    lima_home.mkdir(parents=True, exist_ok=True)
    return {**os.environ, "LIMA_HOME": str(lima_home)}


def _lima_shell(
    script: str, *, distro: str, timeout: int = 120
) -> subprocess.CompletedProcess[str]:
    """Run a bash script inside the test Lima VM for the given distro."""
    return subprocess.run(
        [
            "limactl",
            "shell",
            "--workdir=/tmp",
            _instance_name(distro),
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


@pytest.fixture(scope="module", params=DISTROS)
def lima_vm(request):
    """Create (or reuse) an ephemeral Lima VM for install.sh testing.

    Parameterized across Ubuntu, Fedora, and Arch Linux.  Each VM is
    started once per distro per test module and deleted on teardown.
    """
    distro = request.param
    instance = _instance_name(distro)
    template = TEMPLATES[distro]

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

    # Check if instance already exists
    result = subprocess.run(
        ["limactl", "list", "--format", "{{.Status}}", instance],
        env=env,
        capture_output=True,
        text=True,
    )
    status = result.stdout.strip()

    # Cloud image downloads can be very slow (especially aarch64 Arch Linux
    # which is hosted on a community GitHub repo). Use a generous timeout.
    _VM_START_TIMEOUT = 900

    def _start_lima_vm(extra_args: list[str] | None = None) -> None:
        """Start a new Lima VM, converting failures to pytest.fail()."""
        cmd = [
            "limactl",
            "start",
            *(extra_args or []),
            f"--name={instance}",
            "--tty=false",
            "--mount-writable",
            template,
        ]
        try:
            result = subprocess.run(
                cmd, env=env, capture_output=True, text=True, timeout=_VM_START_TIMEOUT
            )
        except subprocess.TimeoutExpired:
            # Clean up the partially-created instance
            subprocess.run(
                ["limactl", "delete", "--force", instance],
                env=env,
                capture_output=True,
                timeout=60,
            )
            pytest.skip(
                f"Lima VM start for {distro} timed out after {_VM_START_TIMEOUT}s "
                f"(cloud image download may be slow)"
            )
        if result.returncode != 0:
            pytest.fail(
                f"limactl start failed for {distro} (exit {result.returncode}):\n{result.stderr}"
            )

    if not status:
        _start_lima_vm()
    elif status == "Stopped":
        try:
            restart = subprocess.run(
                ["limactl", "start", instance],
                env=env,
                capture_output=True,
                timeout=_VM_START_TIMEOUT,
            )
        except subprocess.TimeoutExpired:
            subprocess.run(
                ["limactl", "delete", "--force", instance],
                env=env,
                capture_output=True,
                timeout=60,
            )
            pytest.skip(
                f"Lima VM restart for {distro} timed out after {_VM_START_TIMEOUT}s"
            )
        if restart.returncode != 0:
            # Instance is corrupted (e.g. leftover from a previous failed run).
            # Delete and recreate.
            subprocess.run(
                ["limactl", "delete", "--force", instance],
                env=env,
                capture_output=True,
                timeout=60,
            )
            _start_lima_vm()

    yield distro

    # Teardown: stop and delete the VM
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

    # Clean up dynamically-generated template files
    if template != f"template:{distro}" and os.path.isfile(template):
        os.unlink(template)


def _copy_to_lima(local_path: Path, guest_path: str, *, distro: str) -> None:
    """Copy a file from the host to the guest VM."""
    subprocess.run(
        ["limactl", "copy", str(local_path), f"{_instance_name(distro)}:{guest_path}"],
        env=_lima_env(),
        check=True,
        timeout=30,
    )


def _guest_home(test_name: str) -> str:
    """Return a unique PEPPY_HOME path on the guest for the given test."""
    return f"/tmp/peppy-test-home/{test_name}"


def _setup_lima_guest(tmp_path: Path, *, test_name: str, distro: str) -> str:
    """Copy install.sh and the fake archive into the Lima guest.

    Returns the guest PEPPY_HOME path for this test.
    """
    archive_path = _create_fake_archive(tmp_path, with_apptainer=True)
    guest_home = _guest_home(test_name)
    # Create a staging dir in the guest
    _lima_shell("mkdir -p /tmp/peppy-test", distro=distro)
    _lima_shell(f"rm -rf {guest_home}", distro=distro)
    _copy_to_lima(INSTALL_SCRIPT, "/tmp/peppy-test/install.sh", distro=distro)
    _copy_to_lima(archive_path, "/tmp/peppy-test/peppy-fake.tgz", distro=distro)
    return guest_home


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


def test_install(lima_vm: str, tmp_path: Path) -> None:
    """Run install.sh in the Lima VM and verify success."""
    distro = lima_vm
    home = _setup_lima_guest(tmp_path, test_name=f"test_install_{distro}", distro=distro)

    result = _lima_shell(
        f"PEPPY_HOME={home} "
        "PEPPY_NO_SERVICE_INSTALL=1 "
        "sh /tmp/peppy-test/install.sh /tmp/peppy-test/peppy-fake.tgz",
        distro=distro,
    )

    diagnostic = f"\n--- stdout ---\n{result.stdout}\n--- stderr ---\n{result.stderr}"

    assert result.returncode == 0, (
        f"install.sh exited with {result.returncode} on {distro}{diagnostic}"
    )
    assert "peppy installed to" in result.stdout, (
        f"Missing 'peppy installed to' in output on {distro}{diagnostic}"
    )

    # Verify the installed binary is executable and runs
    check = _lima_shell(f"test -x {home}/bin/peppy && {home}/bin/peppy", distro=distro)
    assert check.returncode == 0, (
        f"peppy binary should be executable and runnable on {distro}"
        f"\n--- stdout ---\n{check.stdout}\n--- stderr ---\n{check.stderr}"
    )


def test_no_root_install_happy_path(lima_vm: str, tmp_path: Path) -> None:
    """PEPPY_NO_ROOT_INSTALL=1: install succeeds, setuid setup skipped."""
    distro = lima_vm
    home = _setup_lima_guest(
        tmp_path, test_name=f"test_no_root_install_happy_path_{distro}", distro=distro
    )

    result = _lima_shell(
        f"PEPPY_HOME={home} "
        "PEPPY_NO_ROOT_INSTALL=1 "
        "PEPPY_NO_SERVICE_INSTALL=1 "
        "sh /tmp/peppy-test/install.sh /tmp/peppy-test/peppy-fake.tgz",
        distro=distro,
    )

    diagnostic = f"\n--- stdout ---\n{result.stdout}\n--- stderr ---\n{result.stderr}"

    assert result.returncode == 0, (
        f"install.sh exited with {result.returncode} on {distro}{diagnostic}"
    )
    assert "Skipped Apptainer setuid setup" in result.stdout, (
        f"Missing phase 2 skip message on {distro}{diagnostic}"
    )
    assert "peppy installed to" in result.stdout, (
        f"Missing 'peppy installed to' on {distro}{diagnostic}"
    )

    # Verify apptainer directory was extracted but starter-suid is NOT root-owned
    check = _lima_shell(
        f"test -d {home}/bin/apptainer"
        f" && stat -c '%u' {home}/bin/apptainer/libexec/apptainer/bin/starter-suid",
        distro=distro,
    )
    assert check.returncode == 0, f"apptainer dir should exist on {distro}"
    owner_uid = check.stdout.strip()
    assert owner_uid != "0", (
        f"starter-suid should NOT be root-owned on {distro}, but uid is {owner_uid}"
    )


def test_no_root_install_missing_dbus(lima_vm: str, tmp_path: Path) -> None:
    """PEPPY_NO_ROOT_INSTALL=1 with D-Bus session bus unavailable: hard error."""
    distro = lima_vm
    home = _setup_lima_guest(
        tmp_path, test_name=f"test_no_root_install_missing_dbus_{distro}", distro=distro
    )

    # Point DBUS_SESSION_BUS_ADDRESS to a non-existent socket so dbus-send
    # --session fails to connect, simulating a system without a working D-Bus
    # user session. Simply unsetting the variable is not enough because
    # dbus-send falls back to the well-known socket /run/user/UID/bus.
    result = _lima_shell(
        "DBUS_SESSION_BUS_ADDRESS=unix:path=/dev/null/nonexistent "
        f"PEPPY_HOME={home} "
        "PEPPY_NO_ROOT_INSTALL=1 "
        "PEPPY_NO_SERVICE_INSTALL=1 "
        "sh /tmp/peppy-test/install.sh /tmp/peppy-test/peppy-fake.tgz",
        distro=distro,
    )

    output = result.stdout + result.stderr
    diagnostic = f"\n--- stdout ---\n{result.stdout}\n--- stderr ---\n{result.stderr}"

    assert result.returncode != 0, f"install.sh should have failed{diagnostic}"
    assert "D-Bus user session bus is not available" in output, (
        f"error should mention D-Bus user session bus{diagnostic}"
    )


def test_standard_install_sets_up_setuid(lima_vm: str, tmp_path: Path) -> None:
    """Default install (no PEPPY_NO_ROOT_INSTALL): setuid is configured."""
    distro = lima_vm
    home = _setup_lima_guest(
        tmp_path,
        test_name=f"test_standard_install_sets_up_setuid_{distro}",
        distro=distro,
    )

    result = _lima_shell(
        f"PEPPY_HOME={home} "
        "PEPPY_NO_SERVICE_INSTALL=1 "
        "sh /tmp/peppy-test/install.sh /tmp/peppy-test/peppy-fake.tgz",
        distro=distro,
    )

    diagnostic = f"\n--- stdout ---\n{result.stdout}\n--- stderr ---\n{result.stderr}"

    assert result.returncode == 0, (
        f"install.sh exited with {result.returncode} on {distro}{diagnostic}"
    )
    assert "System dependencies configured successfully" in result.stdout, (
        f"Missing system dependencies success message on {distro}{diagnostic}"
    )

    # Verify starter-suid is root-owned with setuid bit
    check = _lima_shell(
        f"stat -c '%u %a' {home}/bin/apptainer/libexec/apptainer/bin/starter-suid",
        distro=distro,
    )
    parts = check.stdout.strip().split()
    assert len(parts) == 2, f"unexpected stat output on {distro}: {check.stdout}"
    uid, mode = parts[0], parts[1]
    assert uid == "0", f"starter-suid should be root-owned on {distro}, got uid {uid}"
    assert mode == "4755", f"starter-suid should have mode 4755 on {distro}, got {mode}"


def test_reinstall_over_root_owned_files(lima_vm: str, tmp_path: Path) -> None:
    """Reinstall succeeds even when previous install left root-owned Apptainer files."""
    distro = lima_vm
    home = _setup_lima_guest(
        tmp_path, test_name=f"test_reinstall_over_root_owned_{distro}", distro=distro
    )

    # First install: creates root-owned apptainer files via setuid setup
    first = _lima_shell(
        f"PEPPY_HOME={home} "
        "PEPPY_NO_SERVICE_INSTALL=1 "
        "PEPPY_FORCE_REINSTALL=1 "
        "sh /tmp/peppy-test/install.sh /tmp/peppy-test/peppy-fake.tgz",
        distro=distro,
    )
    diagnostic = f"\n--- stdout ---\n{first.stdout}\n--- stderr ---\n{first.stderr}"
    assert first.returncode == 0, f"first install failed on {distro}{diagnostic}"

    # Verify root-owned files exist (confirms setuid setup ran)
    check = _lima_shell(
        f"stat -c '%u' {home}/bin/apptainer/etc/apptainer/apptainer.conf",
        distro=distro,
    )
    assert check.stdout.strip() == "0", (
        f"apptainer config should be root-owned after first install on {distro}"
    )

    # Second install: must handle root-owned files without errors
    second = _lima_shell(
        f"PEPPY_HOME={home} "
        "PEPPY_NO_SERVICE_INSTALL=1 "
        "PEPPY_FORCE_REINSTALL=1 "
        "sh /tmp/peppy-test/install.sh /tmp/peppy-test/peppy-fake.tgz",
        distro=distro,
    )
    diagnostic = f"\n--- stdout ---\n{second.stdout}\n--- stderr ---\n{second.stderr}"
    assert second.returncode == 0, f"reinstall failed on {distro}{diagnostic}"
    assert "Permission denied" not in second.stderr, (
        f"reinstall should not produce permission errors on {distro}{diagnostic}"
    )
    assert "peppy installed to" in second.stdout, (
        f"Missing 'peppy installed to' after reinstall on {distro}{diagnostic}"
    )


def test_existing_install_warning(lima_vm: str, tmp_path: Path) -> None:
    """When PEPPY_HOME exists but daemon is not running, show existing install warning."""
    distro = lima_vm
    home = _setup_lima_guest(
        tmp_path, test_name=f"test_existing_install_warning_{distro}", distro=distro
    )

    # Create PEPPY_HOME directory to simulate a previous install
    _lima_shell(f"mkdir -p {home}/bin", distro=distro)

    # Run without PEPPY_FORCE_REINSTALL — non-interactive should fail with
    # the "cannot prompt" error, proving the existing-install check triggered.
    result = _lima_shell(
        f"PEPPY_HOME={home} "
        "PEPPY_NO_SERVICE_INSTALL=1 "
        "sh /tmp/peppy-test/install.sh /tmp/peppy-test/peppy-fake.tgz",
        distro=distro,
    )

    output = result.stdout + result.stderr
    diagnostic = f"\n--- stdout ---\n{result.stdout}\n--- stderr ---\n{result.stderr}"

    assert "An existing installation was found" in output, (
        f"Missing existing-install warning on {distro}{diagnostic}"
    )


def test_unified_sudo_prompt_shows_all_items(lima_vm: str, tmp_path: Path) -> None:
    """The pre-download sudo prompt lists Apptainer items on all distros."""
    distro = lima_vm
    home = _setup_lima_guest(
        tmp_path, test_name=f"test_unified_sudo_prompt_{distro}", distro=distro
    )

    result = _lima_shell(
        f"PEPPY_HOME={home} "
        "PEPPY_NO_SERVICE_INSTALL=1 "
        "sh /tmp/peppy-test/install.sh /tmp/peppy-test/peppy-fake.tgz",
        distro=distro,
    )

    output = result.stdout + result.stderr
    diagnostic = f"\n--- stdout ---\n{result.stdout}\n--- stderr ---\n{result.stderr}"

    assert result.returncode == 0, f"install.sh failed on {distro}{diagnostic}"

    # The single prompt should contain both Apptainer items (present on all distros)
    assert "Set setuid permissions on Apptainer starter binary" in output, (
        f"Missing Apptainer setuid label in prompt on {distro}{diagnostic}"
    )
    assert "Set root ownership on Apptainer configuration" in output, (
        f"Missing Apptainer config ownership label in prompt on {distro}{diagnostic}"
    )

    # These labels must appear BEFORE the download (i.e. before "Extracting archive")
    prompt_pos = output.find("Set setuid permissions on Apptainer")
    extract_pos = output.find("Extracting archive")
    assert prompt_pos < extract_pos, (
        f"Apptainer labels should appear before extraction on {distro}{diagnostic}"
    )
