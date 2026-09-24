"""Tests for scripts/install.sh on the machine the suite runs on.

Each test installs the release archive of this machine's platform the way a
user does, from a login session (see install_helpers), and checks what the
install left behind. They share one machine, so each test first removes the
peppy an earlier test installed and then sets up, explicitly, the system
state it starts from; no test relies on another having run before it.

The tests change the machine, so they run only on a disposable host (see
conftest). The container path of install.sh and its platform check are in
test_install_in_docker.py.
"""

from __future__ import annotations

import subprocess

import pytest

from .install_helpers import (
    diagnostic,
    install_cmd,
    linux_only,
    login_shell,
    peppy_home,
    remove_peppy,
    set_fuse2fs_installed,
    set_linger,
)

# Every test here installs peppy, which is three orders of magnitude dearer
# than the rest of the suite: `pixi run test-fast` deselects this module,
# `pixi run test-install` runs it, and CI runs the two in different jobs.
pytestmark = pytest.mark.install


@pytest.fixture(autouse=True)
def _no_peppy_installed() -> None:
    """Start every test from a machine with no peppy installed or running."""
    remove_peppy()


def _assert_installed(result: subprocess.CompletedProcess[str]) -> None:
    assert result.returncode == 0, (
        f"install.sh exited with {result.returncode}{diagnostic(result)}"
    )
    assert "peppy installed to" in result.stdout, (
        f"Missing 'peppy installed to' in the output{diagnostic(result)}"
    )


def test_install() -> None:
    """install.sh installs a peppy binary that runs."""
    home = peppy_home("install")

    _assert_installed(login_shell(install_cmd(home)))

    check = login_shell(f"test -x {home}/bin/peppy && {home}/bin/peppy --help")
    assert check.returncode == 0, (
        f"peppy binary should be executable and respond to --help{diagnostic(check)}"
    )


@linux_only
def test_install_applies_missing_system_changes() -> None:
    """On a machine without linger or fuse2fs, install.sh sets up both."""
    set_linger(False)
    set_fuse2fs_installed(False)
    home = peppy_home("sysdeps")

    result = login_shell(install_cmd(home), timeout=600)

    _assert_installed(result)
    assert "Enable systemd linger for user" in result.stdout, (
        f"install.sh should ask to enable linger{diagnostic(result)}"
    )
    assert "Install fuse2fs" in result.stdout, (
        f"install.sh should ask to install fuse2fs{diagnostic(result)}"
    )
    assert "Pre-download dependencies configured." in result.stdout, (
        f"install.sh should report the package install{diagnostic(result)}"
    )
    state = login_shell(
        'command -v fuse2fs && loginctl show-user "$(id -un)" -p Linger --value'
    )
    assert state.returncode == 0 and state.stdout.strip().endswith("yes"), (
        f"fuse2fs should be installed and linger enabled{diagnostic(state)}"
    )


@linux_only
def test_no_root_install_happy_path() -> None:
    """PEPPY_NO_ROOT_INSTALL=1 with every prerequisite met: install succeeds, container setup skipped."""
    set_linger(True)
    set_fuse2fs_installed(True)
    home = peppy_home("noroot")

    result = login_shell(install_cmd(home, extra_env="PEPPY_NO_ROOT_INSTALL=1"))

    _assert_installed(result)
    assert "Skipped Apptainer setup" in result.stdout, (
        f"Missing setup skip message{diagnostic(result)}"
    )
    check = login_shell(
        f"test -d {home}/bin/apptainer && test -f {home}/bin/apptainer/bin/apptainer"
    )
    assert check.returncode == 0, (
        f"apptainer dir and binary should exist{diagnostic(check)}"
    )


@linux_only
def test_no_root_install_missing_dbus() -> None:
    """PEPPY_NO_ROOT_INSTALL=1 with the D-Bus user bus unreachable: hard error."""
    home = peppy_home("nodbus")

    # Point DBUS_SESSION_BUS_ADDRESS at a socket that cannot exist so the bus
    # probe fails to connect. Unsetting the variable is not enough, because
    # the probe falls back to the well-known socket /run/user/UID/bus.
    result = login_shell(
        install_cmd(
            home,
            extra_env=(
                "DBUS_SESSION_BUS_ADDRESS=unix:path=/dev/null/nonexistent "
                "PEPPY_NO_ROOT_INSTALL=1"
            ),
        )
    )

    assert result.returncode != 0, f"install.sh should have failed{diagnostic(result)}"
    assert "D-Bus user session bus is not available" in result.stdout + result.stderr, (
        f"error should mention D-Bus user session bus{diagnostic(result)}"
    )


@linux_only
def test_standard_install_container_setup() -> None:
    """Default install: the Apptainer setup runs and leaves its prerequisites met."""
    home = peppy_home("ctsetup")

    _assert_installed(login_shell(install_cmd(home), timeout=600))

    # No starter-suid in --without-suid builds.
    starter = login_shell(f"test -f {home}/bin/apptainer/libexec/apptainer/bin/starter")
    assert starter.returncode == 0, (
        f"apptainer starter binary should exist{diagnostic(starter)}"
    )
    status = login_shell(f"PEPPY_HOME={home} {home}/bin/peppy container status")
    assert status.returncode == 0, (
        f"every Apptainer prerequisite should pass after the install{diagnostic(status)}"
    )


@linux_only
def test_install_skips_container_setup_with_env_var() -> None:
    """PEPPY_NO_CONTAINER_SETUP=1 explicitly skips Apptainer container setup."""
    home = peppy_home("noctsetup")

    result = login_shell(install_cmd(home, extra_env="PEPPY_NO_CONTAINER_SETUP=1"))

    _assert_installed(result)
    assert "Skipped Apptainer setup (PEPPY_NO_CONTAINER_SETUP is set)" in result.stdout, (
        f"Missing env var skip message{diagnostic(result)}"
    )
    check = login_shell(f"test -x {home}/bin/peppy")
    assert check.returncode == 0, f"peppy binary not found at {home}/bin/peppy"


def test_install_runs_repo_update() -> None:
    """Full install (with service) runs 'peppy repo update' after the daemon starts."""
    home = peppy_home("service")

    result = login_shell(install_cmd(home, skip_service_install=False), timeout=600)

    _assert_installed(result)
    output = result.stdout + result.stderr
    assert (
        "Refreshing repositories" in output or "Repository refresh complete" in output
    ), f"Missing repo update output{diagnostic(result)}"


def test_reinstall_over_existing() -> None:
    """Reinstall succeeds over an existing installation and keeps PEPPY_HOME."""
    home = peppy_home("reinstall")

    _assert_installed(login_shell(install_cmd(home)))
    marker = login_shell(f"echo marker > {home}/test_marker.txt")
    assert marker.returncode == 0, f"could not write the marker{diagnostic(marker)}"

    # Second install: overwrites the binaries in place.
    _assert_installed(login_shell(install_cmd(home)))

    check = login_shell(f"cat {home}/test_marker.txt")
    assert check.returncode == 0 and "marker" in check.stdout, (
        f"marker file should survive reinstall{diagnostic(check)}"
    )


@linux_only
def test_container_node_build() -> None:
    """Container node workflow on the installed peppy: init, add and build, run.

    The install includes the service, so the daemon the node commands talk to
    is the one a user gets, and install.sh has already waited for it to answer.
    """
    home = peppy_home("ctnode")
    env_preamble = f'export PATH="{home}/bin:$PATH" && export PEPPY_HOME={home}'

    _assert_installed(
        login_shell(install_cmd(home, skip_service_install=False), timeout=600)
    )

    init = login_shell(f"{env_preamble} && cd {home} && peppy node init --container test-node")
    assert init.returncode == 0, f"node init failed{diagnostic(init)}"

    # Builds the node's Apptainer container.
    add = login_shell(
        f"{env_preamble} && cd {home}/test-node && peppy node add . --build",
        timeout=900,
    )
    assert add.returncode == 0, f"node add failed{diagnostic(add)}"

    run = login_shell(f"{env_preamble} && peppy node run test-node:v1")
    assert run.returncode == 0, f"node run failed{diagnostic(run)}"
