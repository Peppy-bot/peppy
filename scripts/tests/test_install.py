"""Tests for scripts/install.sh.

All tests run inside a Lima VM (Ubuntu 24.04) as a non-root sudo user to
replicate a real-world install scenario.
"""

from __future__ import annotations

import io
import os
import subprocess
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
        ["limactl", "shell", _LIMA_INSTANCE, "--", "bash", "-c", script],
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
    env = _lima_env()

    # Check if instance already exists
    result = subprocess.run(
        ["limactl", "list", "--format", "{{.Status}}", _LIMA_INSTANCE],
        env=env,
        capture_output=True,
        text=True,
    )
    status = result.stdout.strip()

    if not status:
        # Create new instance
        subprocess.run(
            [
                "limactl", "start",
                f"--name={_LIMA_INSTANCE}",
                "--tty=false",
                "--mount-writable",
                "template:ubuntu-24.04",
            ],
            env=env,
            check=True,
            timeout=300,
        )
    elif status == "Stopped":
        subprocess.run(
            ["limactl", "start", _LIMA_INSTANCE],
            env=env,
            check=True,
            timeout=300,
        )

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


def _setup_lima_guest(tmp_path: Path) -> None:
    """Copy install.sh and the fake archive into the Lima guest."""
    archive_path = _create_fake_archive(tmp_path, with_apptainer=True)
    # Create a staging dir in the guest
    _lima_shell("mkdir -p /tmp/peppy-test")
    _copy_to_lima(INSTALL_SCRIPT, "/tmp/peppy-test/install.sh")
    _copy_to_lima(archive_path, "/tmp/peppy-test/peppy-fake.tgz")


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

def test_install(lima_vm, tmp_path: Path) -> None:
    """Run install.sh in the Lima VM and verify success."""
    _setup_lima_guest(tmp_path)

    # Clean previous install
    _lima_shell("rm -rf ~/.peppy")

    result = _lima_shell(
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
    check = _lima_shell(
        "test -x ~/.peppy/bin/peppy && ~/.peppy/bin/peppy"
    )
    assert check.returncode == 0, (
        f"peppy binary should be executable and runnable"
        f"\n--- stdout ---\n{check.stdout}\n--- stderr ---\n{check.stderr}"
    )


def test_no_root_install_happy_path(lima_vm, tmp_path: Path) -> None:
    """PEPPY_NO_ROOT_INSTALL=1: install succeeds, setuid setup skipped."""
    _setup_lima_guest(tmp_path)

    result = _lima_shell(
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
        "test -d ~/.peppy/bin/apptainer"
        " && stat -c '%u' ~/.peppy/bin/apptainer/libexec/apptainer/bin/starter-suid"
    )
    assert check.returncode == 0, "apptainer dir should exist"
    owner_uid = check.stdout.strip()
    assert owner_uid != "0", (
        f"starter-suid should NOT be root-owned, but uid is {owner_uid}"
    )


def test_no_root_install_missing_dbus(lima_vm, tmp_path: Path) -> None:
    """PEPPY_NO_ROOT_INSTALL=1 with dbus-user-session removed: hard error."""
    _setup_lima_guest(tmp_path)

    result = _lima_shell(
        "sudo apt-get remove -y -qq dbus-user-session > /dev/null 2>&1; "
        "PEPPY_NO_ROOT_INSTALL=1 "
        "PEPPY_NO_SERVICE_INSTALL=1 "
        "sh /tmp/peppy-test/install.sh /tmp/peppy-test/peppy-fake.tgz"
    )

    diagnostic = f"\n--- stdout ---\n{result.stdout}\n--- stderr ---\n{result.stderr}"

    assert result.returncode != 0, (
        f"install.sh should have failed{diagnostic}"
    )
    assert "dbus-user-session" in result.stderr, (
        f"error should mention dbus-user-session{diagnostic}"
    )

    # Restore dbus-user-session for subsequent tests
    _lima_shell("sudo apt-get install -y -qq dbus-user-session > /dev/null 2>&1")


def test_standard_install_sets_up_setuid(lima_vm, tmp_path: Path) -> None:
    """Default install (no PEPPY_NO_ROOT_INSTALL): setuid is configured."""
    _setup_lima_guest(tmp_path)

    # Clean previous install
    _lima_shell("rm -rf ~/.peppy")

    result = _lima_shell(
        "PEPPY_NO_SERVICE_INSTALL=1 "
        "sh /tmp/peppy-test/install.sh /tmp/peppy-test/peppy-fake.tgz"
    )

    diagnostic = f"\n--- stdout ---\n{result.stdout}\n--- stderr ---\n{result.stderr}"

    assert result.returncode == 0, (
        f"install.sh exited with {result.returncode}{diagnostic}"
    )
    assert "Apptainer setuid configured successfully" in result.stdout, (
        f"Missing setuid success message{diagnostic}"
    )

    # Verify starter-suid is root-owned with setuid bit
    check = _lima_shell(
        "stat -c '%u %a' ~/.peppy/bin/apptainer/libexec/apptainer/bin/starter-suid"
    )
    parts = check.stdout.strip().split()
    assert len(parts) == 2, f"unexpected stat output: {check.stdout}"
    uid, mode = parts[0], parts[1]
    assert uid == "0", f"starter-suid should be root-owned, got uid {uid}"
    assert mode == "4755", f"starter-suid should have mode 4755, got {mode}"
