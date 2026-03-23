"""Tests for scripts/install.sh.

All tests run inside a Lima VM (Ubuntu 24.04) as a non-root sudo user to
replicate a real-world install scenario.
"""

from __future__ import annotations

import io
import os
import shutil
import subprocess
import sys
import tarfile
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
INSTALL_SCRIPT = REPO_ROOT / "scripts" / "install.sh"

_LIMA_INSTANCE = "peppy-test-install"


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


def _lima_shell(script: str, *, timeout: int = 120) -> subprocess.CompletedProcess[str]:
    """Run a bash script inside the test Lima VM."""
    return subprocess.run(
        [
            "limactl",
            "shell",
            "--workdir=/tmp",
            _LIMA_INSTANCE,
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


@pytest.fixture(scope="module")
def lima_vm():
    """Create (or reuse) an ephemeral Lima VM for install.sh testing.

    The VM is started once per test module and deleted on teardown.
    """
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
        ["limactl", "list", "--format", "{{.Status}}", _LIMA_INSTANCE],
        env=env,
        capture_output=True,
        text=True,
    )
    status = result.stdout.strip()

    def _start_lima_vm(extra_args: list[str] | None = None) -> None:
        """Start a new Lima VM, converting failures to pytest.fail()."""
        cmd = [
            "limactl",
            "start",
            *(extra_args or []),
            f"--name={_LIMA_INSTANCE}",
            "--tty=false",
            "--mount-writable",
            "template:ubuntu-24.04",
        ]
        result = subprocess.run(
            cmd, env=env, capture_output=True, text=True, timeout=300
        )
        if result.returncode != 0:
            pytest.fail(
                f"limactl start failed (exit {result.returncode}):\n{result.stderr}"
            )

    if not status:
        _start_lima_vm()
    elif status == "Stopped":
        restart = subprocess.run(
            ["limactl", "start", _LIMA_INSTANCE],
            env=env,
            capture_output=True,
            timeout=300,
        )
        if restart.returncode != 0:
            # Instance is corrupted (e.g. leftover from a previous failed run).
            # Delete and recreate.
            subprocess.run(
                ["limactl", "delete", "--force", _LIMA_INSTANCE],
                env=env,
                capture_output=True,
                timeout=60,
            )
            _start_lima_vm()

    yield

    # Teardown: stop and delete the VM
    subprocess.run(
        ["limactl", "stop", _LIMA_INSTANCE],
        env=env,
        capture_output=True,
        timeout=60,
    )
    subprocess.run(
        ["limactl", "delete", _LIMA_INSTANCE],
        env=env,
        capture_output=True,
        timeout=60,
    )


def _copy_to_lima(local_path: Path, guest_path: str) -> None:
    """Copy a file from the host to the guest VM."""
    subprocess.run(
        ["limactl", "copy", str(local_path), f"{_LIMA_INSTANCE}:{guest_path}"],
        env=_lima_env(),
        check=True,
        timeout=30,
    )


def _guest_home(test_name: str) -> str:
    """Return a unique PEPPY_HOME path on the guest for the given test."""
    return f"/tmp/peppy-test-home/{test_name}"


def _setup_lima_guest(tmp_path: Path, *, test_name: str) -> str:
    """Copy install.sh and the fake archive into the Lima guest.

    Returns the guest PEPPY_HOME path for this test.
    """
    archive_path = _create_fake_archive(tmp_path, with_apptainer=True)
    guest_home = _guest_home(test_name)
    # Create a staging dir in the guest
    _lima_shell("mkdir -p /tmp/peppy-test")
    _lima_shell(f"rm -rf {guest_home}")
    _copy_to_lima(INSTALL_SCRIPT, "/tmp/peppy-test/install.sh")
    _copy_to_lima(archive_path, "/tmp/peppy-test/peppy-fake.tgz")
    return guest_home


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


def test_install(lima_vm, tmp_path: Path) -> None:
    """Run install.sh in the Lima VM and verify success."""
    home = _setup_lima_guest(tmp_path, test_name="test_install")

    result = _lima_shell(
        f"PEPPY_HOME={home} "
        "PEPPY_NO_SERVICE_INSTALL=1 "
        "sh /tmp/peppy-test/install.sh /tmp/peppy-test/peppy-fake.tgz"
    )

    diagnostic = f"\n--- stdout ---\n{result.stdout}\n--- stderr ---\n{result.stderr}"

    assert result.returncode == 0, (
        f"install.sh exited with {result.returncode}{diagnostic}"
    )
    assert "peppy installed to" in result.stdout, (
        f"Missing 'peppy installed to' in output{diagnostic}"
    )

    # Verify the installed binary is executable and runs
    check = _lima_shell(f"test -x {home}/bin/peppy && {home}/bin/peppy")
    assert check.returncode == 0, (
        f"peppy binary should be executable and runnable"
        f"\n--- stdout ---\n{check.stdout}\n--- stderr ---\n{check.stderr}"
    )


def test_no_root_install_happy_path(lima_vm, tmp_path: Path) -> None:
    """PEPPY_NO_ROOT_INSTALL=1: install succeeds, setuid setup skipped."""
    home = _setup_lima_guest(tmp_path, test_name="test_no_root_install_happy_path")

    result = _lima_shell(
        f"PEPPY_HOME={home} "
        "PEPPY_NO_ROOT_INSTALL=1 "
        "PEPPY_NO_SERVICE_INSTALL=1 "
        "sh /tmp/peppy-test/install.sh /tmp/peppy-test/peppy-fake.tgz"
    )

    diagnostic = f"\n--- stdout ---\n{result.stdout}\n--- stderr ---\n{result.stderr}"

    assert result.returncode == 0, (
        f"install.sh exited with {result.returncode}{diagnostic}"
    )
    assert "Skipped Apptainer setuid setup" in result.stdout, (
        f"Missing phase 2 skip message{diagnostic}"
    )
    assert "peppy installed to" in result.stdout, (
        f"Missing 'peppy installed to'{diagnostic}"
    )

    # Verify apptainer directory was extracted but starter-suid is NOT root-owned
    check = _lima_shell(
        f"test -d {home}/bin/apptainer"
        f" && stat -c '%u' {home}/bin/apptainer/libexec/apptainer/bin/starter-suid"
    )
    assert check.returncode == 0, "apptainer dir should exist"
    owner_uid = check.stdout.strip()
    assert owner_uid != "0", (
        f"starter-suid should NOT be root-owned, but uid is {owner_uid}"
    )


def test_no_root_install_missing_dbus(lima_vm, tmp_path: Path) -> None:
    """PEPPY_NO_ROOT_INSTALL=1 with dbus-user-session removed: hard error."""
    home = _setup_lima_guest(tmp_path, test_name="test_no_root_install_missing_dbus")

    result = _lima_shell(
        "sudo apt-get purge -y -qq dbus-user-session > /dev/null 2>&1; "
        f"PEPPY_HOME={home} "
        "PEPPY_NO_ROOT_INSTALL=1 "
        "PEPPY_NO_SERVICE_INSTALL=1 "
        "sh /tmp/peppy-test/install.sh /tmp/peppy-test/peppy-fake.tgz"
    )

    output = result.stdout + result.stderr
    diagnostic = f"\n--- stdout ---\n{result.stdout}\n--- stderr ---\n{result.stderr}"

    assert result.returncode != 0, f"install.sh should have failed{diagnostic}"
    assert "dbus-user-session" in output, (
        f"error should mention dbus-user-session{diagnostic}"
    )

    # Restore dbus-user-session for subsequent tests
    _lima_shell("sudo apt-get install -y -qq dbus-user-session > /dev/null 2>&1")


def test_standard_install_sets_up_setuid(lima_vm, tmp_path: Path) -> None:
    """Default install (no PEPPY_NO_ROOT_INSTALL): setuid is configured."""
    home = _setup_lima_guest(tmp_path, test_name="test_standard_install_sets_up_setuid")

    result = _lima_shell(
        f"PEPPY_HOME={home} "
        "PEPPY_NO_SERVICE_INSTALL=1 "
        "sh /tmp/peppy-test/install.sh /tmp/peppy-test/peppy-fake.tgz"
    )

    diagnostic = f"\n--- stdout ---\n{result.stdout}\n--- stderr ---\n{result.stderr}"

    assert result.returncode == 0, (
        f"install.sh exited with {result.returncode}{diagnostic}"
    )
    assert "System dependencies configured successfully" in result.stdout, (
        f"Missing system dependencies success message{diagnostic}"
    )

    # Verify starter-suid is root-owned with setuid bit
    check = _lima_shell(
        f"stat -c '%u %a' {home}/bin/apptainer/libexec/apptainer/bin/starter-suid"
    )
    parts = check.stdout.strip().split()
    assert len(parts) == 2, f"unexpected stat output: {check.stdout}"
    uid, mode = parts[0], parts[1]
    assert uid == "0", f"starter-suid should be root-owned, got uid {uid}"
    assert mode == "4755", f"starter-suid should have mode 4755, got {mode}"


def test_reinstall_over_root_owned_files(lima_vm, tmp_path: Path) -> None:
    """Reinstall succeeds even when previous install left root-owned Apptainer files."""
    home = _setup_lima_guest(tmp_path, test_name="test_reinstall_over_root_owned")

    # First install: creates root-owned apptainer files via setuid setup
    first = _lima_shell(
        f"PEPPY_HOME={home} "
        "PEPPY_NO_SERVICE_INSTALL=1 "
        "PEPPY_FORCE_REINSTALL=1 "
        "sh /tmp/peppy-test/install.sh /tmp/peppy-test/peppy-fake.tgz"
    )
    diagnostic = f"\n--- stdout ---\n{first.stdout}\n--- stderr ---\n{first.stderr}"
    assert first.returncode == 0, f"first install failed{diagnostic}"

    # Verify root-owned files exist (confirms setuid setup ran)
    check = _lima_shell(
        f"stat -c '%u' {home}/bin/apptainer/etc/apptainer/apptainer.conf"
    )
    assert check.stdout.strip() == "0", "apptainer config should be root-owned after first install"

    # Second install: must handle root-owned files without errors
    second = _lima_shell(
        f"PEPPY_HOME={home} "
        "PEPPY_NO_SERVICE_INSTALL=1 "
        "PEPPY_FORCE_REINSTALL=1 "
        "sh /tmp/peppy-test/install.sh /tmp/peppy-test/peppy-fake.tgz"
    )
    diagnostic = f"\n--- stdout ---\n{second.stdout}\n--- stderr ---\n{second.stderr}"
    assert second.returncode == 0, f"reinstall failed{diagnostic}"
    assert "Permission denied" not in second.stderr, (
        f"reinstall should not produce permission errors{diagnostic}"
    )
    assert "peppy installed to" in second.stdout, (
        f"Missing 'peppy installed to' after reinstall{diagnostic}"
    )


def test_existing_install_warning(lima_vm, tmp_path: Path) -> None:
    """When PEPPY_HOME exists but daemon is not running, show existing install warning."""
    home = _setup_lima_guest(tmp_path, test_name="test_existing_install_warning")

    # Create PEPPY_HOME directory to simulate a previous install
    _lima_shell(f"mkdir -p {home}/bin")

    # Run without PEPPY_FORCE_REINSTALL — non-interactive should fail with
    # the "cannot prompt" error, proving the existing-install check triggered.
    result = _lima_shell(
        f"PEPPY_HOME={home} "
        "PEPPY_NO_SERVICE_INSTALL=1 "
        "sh /tmp/peppy-test/install.sh /tmp/peppy-test/peppy-fake.tgz"
    )

    output = result.stdout + result.stderr
    diagnostic = f"\n--- stdout ---\n{result.stdout}\n--- stderr ---\n{result.stderr}"

    assert "An existing installation was found" in output, (
        f"Missing existing-install warning{diagnostic}"
    )


def test_unified_sudo_prompt_shows_all_items(lima_vm, tmp_path: Path) -> None:
    """The pre-download sudo prompt lists both system deps and Apptainer items."""
    home = _setup_lima_guest(tmp_path, test_name="test_unified_sudo_prompt")

    result = _lima_shell(
        f"PEPPY_HOME={home} "
        "PEPPY_NO_SERVICE_INSTALL=1 "
        "sh /tmp/peppy-test/install.sh /tmp/peppy-test/peppy-fake.tgz"
    )

    output = result.stdout + result.stderr
    diagnostic = f"\n--- stdout ---\n{result.stdout}\n--- stderr ---\n{result.stderr}"

    assert result.returncode == 0, f"install.sh failed{diagnostic}"

    # The single prompt should contain both Apptainer items
    assert "Set setuid permissions on Apptainer starter binary" in output, (
        f"Missing Apptainer setuid label in prompt{diagnostic}"
    )
    assert "Set root ownership on Apptainer configuration" in output, (
        f"Missing Apptainer config ownership label in prompt{diagnostic}"
    )

    # These labels must appear BEFORE the download (i.e. before "Extracting archive")
    prompt_pos = output.find("Set setuid permissions on Apptainer")
    extract_pos = output.find("Extracting archive")
    assert prompt_pos < extract_pos, (
        f"Apptainer labels should appear before extraction{diagnostic}"
    )
