"""Tests for the shared Lima VM helpers.

Host-side control flow only: no VM is booted here. The VM-backed suites that
use these helpers live in test_install.py and test_install_container.py.
"""

from __future__ import annotations

import os
import subprocess
from contextlib import contextmanager
from pathlib import Path
from typing import TYPE_CHECKING, Any
from unittest.mock import patch

import pytest

from .lima_helpers import (
    _UBUNTU_PROVISION_SCRIPT,
    VMConfig,
    host_arch,
    lima_vm_lifecycle,
    provision_guest,
)

if TYPE_CHECKING:
    from collections.abc import Iterator

# What pytest.fail raises. It derives from BaseException, so a bare
# `pytest.raises(Exception)` would sail straight past it.
Failed = pytest.fail.Exception


def _config(distro: str = "ubuntu") -> VMConfig:
    """A native-arch config, so none of the cross-arch branches are taken."""
    return VMConfig(os="linux", arch=host_arch(), distro=distro, instance_prefix="ptt")


class _FakeRequest:
    """Stand-in for pytest's FixtureRequest: the fixture only reads .param."""

    def __init__(self, param: VMConfig) -> None:
        self.param = param


@contextmanager
def _fake_lima_host(
    calls: list[list[str]],
    tmp_path: Path,
    *,
    provision_returncode: int = 0,
    verify_returncode: int = 0,
    keep_failed_vm: bool = False,
    timeout_on: str | None = None,
) -> Iterator[None]:
    """Answer every limactl call in-process, recording each one's argv.

    *timeout_on* is a limactl subcommand that hangs instead of answering.
    Shell calls are told apart by their script: the baseline verification is
    the one probing the bus, everything else answers as provisioning.

    Also neutralises the host virtualisation preconditions, which would
    otherwise fail the fixture on a machine without qemu or /dev/kvm.
    """

    def run(cmd: list[str], **_: Any) -> subprocess.CompletedProcess[str]:
        calls.append(cmd)
        if cmd[1] == timeout_on:
            raise subprocess.TimeoutExpired(cmd, 900)
        if cmd[1] == "shell":
            if "busctl --user status" in cmd[-1]:
                return subprocess.CompletedProcess(
                    cmd, verify_returncode, "", "XDG_RUNTIME_DIR=unset"
                )
            return subprocess.CompletedProcess(
                cmd,
                provision_returncode,
                "",
                "Could not enable linger: Connection timed out",
            )
        return subprocess.CompletedProcess(cmd, 0, "", "")

    environ = {"PEPPY_TEST_LIMA_HOME": str(tmp_path)}
    if keep_failed_vm:
        environ["PEPPY_TEST_KEEP_VM"] = "1"

    with (
        patch.dict(os.environ, environ),
        patch("tests.lima_helpers.subprocess.run", side_effect=run),
        patch("tests.lima_helpers.shutil.which", return_value="/usr/bin/qemu-img"),
        patch("tests.lima_helpers.os.path.exists", return_value=False),
    ):
        if not keep_failed_vm:
            os.environ.pop("PEPPY_TEST_KEEP_VM", None)
        yield


def test_failed_setup_deletes_the_guest(tmp_path: Path) -> None:
    """A fixture that raises before its yield never runs the code after it.

    Provisioning failed for real in CI, and because the teardown lived past the
    yield the booted VM stayed up for the rest of the job -- the runner's
    orphan-process sweep is what finally reaped its qemu.
    """
    calls: list[list[str]] = []
    config = _config()

    with _fake_lima_host(calls, tmp_path, provision_returncode=1):
        lifecycle = lima_vm_lifecycle(_FakeRequest(config))
        with pytest.raises(Failed, match="Ubuntu guest provisioning failed"):
            next(lifecycle)

    assert ["limactl", "delete", "--force", config.instance_name] in calls


def test_failed_setup_keeps_the_guest_when_asked(tmp_path: Path) -> None:
    """PEPPY_TEST_KEEP_VM leaves the wedged guest up: it is the evidence."""
    calls: list[list[str]] = []
    config = _config()

    with _fake_lima_host(calls, tmp_path, provision_returncode=1, keep_failed_vm=True):
        lifecycle = lima_vm_lifecycle(_FakeRequest(config))
        with pytest.raises(Failed):
            next(lifecycle)

    assert [cmd for cmd in calls if cmd[1] == "shell"], (
        "never got as far as provisioning"
    )
    assert not [cmd for cmd in calls if cmd[1] in ("stop", "delete")]


def test_a_finished_module_stops_and_deletes_the_guest(tmp_path: Path) -> None:
    """The happy path still tears down once the tests are done with the VM."""
    calls: list[list[str]] = []
    config = _config()

    with _fake_lima_host(calls, tmp_path):
        lifecycle = lima_vm_lifecycle(_FakeRequest(config))
        assert next(lifecycle) is config
        assert not [cmd for cmd in calls if cmd[1] in ("stop", "delete")]
        with pytest.raises(StopIteration):
            next(lifecycle)

    assert ["limactl", "stop", config.instance_name] in calls
    assert ["limactl", "delete", "--force", config.instance_name] in calls


def test_a_start_that_times_out_deletes_the_guest(tmp_path: Path) -> None:
    """Every way out of setup goes through the same teardown.

    A `limactl start` killed on its timeout still leaves the instance -- and
    the disk image it was writing -- behind on the host.
    """
    calls: list[list[str]] = []
    config = _config()

    with _fake_lima_host(calls, tmp_path, timeout_on="start"):
        lifecycle = lima_vm_lifecycle(_FakeRequest(config))
        with pytest.raises(Failed, match="timed out after"):
            next(lifecycle)

    assert ["limactl", "delete", "--force", config.instance_name] in calls


def test_non_ubuntu_guests_are_not_provisioned(tmp_path: Path) -> None:
    """Fedora and Arch ship a working user session bus, so they are left alone."""
    calls: list[list[str]] = []

    with _fake_lima_host(calls, tmp_path, provision_returncode=1):
        provision_guest(_config("fedora"), dict(os.environ))

    assert calls == []


def test_provisioning_orders_linger_around_the_logind_restart() -> None:
    """Order matters more than any single command in that script.

    `limactl start` returns once ssh answers, while PID 1 may still be working
    through its boot transaction, so the settle waits come first. The premise
    guard (the image ships the user dbus.socket unit) precedes any mutation.
    The linger flag file must exist before logind restarts (a fresh logind
    enumerates the linger directory at startup), the user manager restarts
    after logind (so its startup postdates everything the boot raced), and
    the enable-linger verification comes last (a wedged boot-time instance is
    the one that cannot answer it). Nothing installs packages: the image
    already ships dbus-user-session, and the guard is what says so.
    """
    script = _UBUNTU_PROVISION_SCRIPT

    guard = script.index("/usr/lib/systemd/user/dbus.socket")
    flag_file = script.index("/var/lib/systemd/linger")
    logind = script.index("systemctl restart systemd-logind")
    manager = script.index('systemctl restart "user@')
    verify = script.index("loginctl enable-linger")
    assert script.index("cloud-init status --wait") < guard
    assert script.index("is-system-running --wait") < guard
    assert guard < flag_file < logind < manager < verify
    assert "apt-get" not in script


def test_provisioning_verifies_the_baseline_over_a_fresh_connection(
    tmp_path: Path,
) -> None:
    """The bus check must not ride the connection provisioning used.

    sshd computes the session environment once per multiplexed connection, so
    a master opened against the boot-time logind would feed the check (and
    every test after it) whatever pam_systemd got back then, no matter what
    the restarts fixed. Provisioning has to drop the master between its two
    scripts so the check sees what the tests will see.
    """
    calls: list[list[str]] = []

    with _fake_lima_host(calls, tmp_path):
        provision_guest(_config(), dict(os.environ))

    shells = [cmd for cmd in calls if cmd[0] == "limactl" and cmd[1] == "shell"]
    assert len(shells) == 2
    assert "loginctl enable-linger" in shells[0][-1]
    assert "busctl --user status" in shells[1][-1]

    master_drops = [
        cmd for cmd in calls if cmd[0] == "ssh" and cmd[1:3] == ["-O", "exit"]
    ]
    assert len(master_drops) == 1
    assert calls.index(shells[0]) < calls.index(master_drops[0]) < calls.index(
        shells[1]
    )


def test_a_failed_baseline_check_fails_provisioning_with_the_guest_state(
    tmp_path: Path,
) -> None:
    """A guest whose fresh sessions lack the bus is caught before any test.

    The check's diagnostics name the broken link (here the session
    environment), so the failure reads as what it is instead of surfacing
    twenty tests later as install.sh's missing D-Bus error.
    """
    calls: list[list[str]] = []

    with _fake_lima_host(calls, tmp_path, verify_returncode=1):
        with pytest.raises(Failed, match="baseline verification failed") as excinfo:
            provision_guest(_config(), dict(os.environ))

    assert "XDG_RUNTIME_DIR=unset" in str(excinfo.value)


def test_provisioning_reports_captured_output_when_it_times_out(
    tmp_path: Path,
) -> None:
    """A timeout must still surface what the guest managed to say.

    subprocess hands the partial streams back on the exception as bytes even in
    text mode, so an unformatted one would reach the report as b'...'.
    """

    def run(cmd: list[str], **_: Any) -> subprocess.CompletedProcess[str]:
        raise subprocess.TimeoutExpired(
            cmd, 900, output=b"waiting for logind\n", stderr=b"still booting\n"
        )

    with (
        patch.dict(os.environ, {"PEPPY_TEST_LIMA_HOME": str(tmp_path)}),
        patch("tests.lima_helpers.subprocess.run", side_effect=run),
    ):
        with pytest.raises(Failed) as excinfo:
            provision_guest(_config(), dict(os.environ))

    message = str(excinfo.value)
    assert "did not finish within 600s" in message
    assert "waiting for logind" in message
    assert "still booting" in message
