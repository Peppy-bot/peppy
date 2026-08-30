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
    keep_failed_vm: bool = False,
    timeout_on: str | None = None,
) -> Iterator[None]:
    """Answer every limactl call in-process, recording each one's argv.

    *timeout_on* is a limactl subcommand that hangs instead of answering.

    Also neutralises the host virtualisation preconditions, which would
    otherwise fail the fixture on a machine without qemu or /dev/kvm.
    """

    def run(cmd: list[str], **_: Any) -> subprocess.CompletedProcess[str]:
        calls.append(cmd)
        if cmd[1] == timeout_on:
            raise subprocess.TimeoutExpired(cmd, 900)
        if cmd[1] == "shell":
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
    through its boot transaction, so the settle waits come first. The linger
    flag file must exist before logind restarts (a fresh logind enumerates the
    linger directory at startup), the restart must precede the enable-linger
    verification (a wedged boot-time instance is the one that cannot answer
    it), and the verification must precede the dbus install, which can restart
    the system bus.
    """
    script = _UBUNTU_PROVISION_SCRIPT

    flag_file = script.index("/var/lib/systemd/linger")
    restart = script.index("systemctl restart systemd-logind")
    verify = script.index("loginctl enable-linger")
    assert script.index("cloud-init status --wait") < flag_file
    assert script.index("is-system-running --wait") < flag_file
    assert flag_file < restart < verify < script.index("apt-get install")


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
    assert "did not finish within 900s" in message
    assert "waiting for logind" in message
    assert "still booting" in message
