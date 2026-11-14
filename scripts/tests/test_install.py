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
    IS_LINUX,
    SERVICE_RUNNING_MARK,
    SERVICE_STATUS_COMMAND,
    assert_installed,
    daemon_logs,
    diagnostic,
    install_cmd,
    linux_only,
    login_shell,
    peppy_environment,
    peppy_home,
    pin_default_hubs,
    remove_peppy,
    set_fuse2fs_installed,
    set_linger,
    show_step,
)

# Every test here installs peppy, which is three orders of magnitude dearer
# than the rest of the suite: `pixi run test-fast` deselects this module,
# `pixi run test-install` runs it, and CI runs the two in different jobs.
pytestmark = pytest.mark.install

# The launch of the quickstart (docs/src/content/docs/quickstart.mdx, step 2).
QUICKSTART_LAUNCH = "peppy stack launch openarm_simulation --with mujoco"
# Ends a launch that hangs. A machine that peppy was just installed on has no
# cache, so each node of the launch builds from nothing.
QUICKSTART_LAUNCH_TIMEOUT_SECONDS = 90 * 60


@pytest.fixture(autouse=True)
def _no_peppy_installed() -> None:
    """Start every test from a machine with no peppy installed or running."""
    remove_peppy()


def test_install() -> None:
    """install.sh installs a peppy binary that runs."""
    home = peppy_home("install")

    assert_installed(login_shell(install_cmd(home)))

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

    assert_installed(result)
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

    assert_installed(result)
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

    assert_installed(login_shell(install_cmd(home), timeout=600))

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

    assert_installed(result)
    assert (
        "Skipped Apptainer setup (PEPPY_NO_CONTAINER_SETUP is set)" in result.stdout
    ), f"Missing env var skip message{diagnostic(result)}"
    check = login_shell(f"test -x {home}/bin/peppy")
    assert check.returncode == 0, f"peppy binary not found at {home}/bin/peppy"


def test_install_runs_repo_update() -> None:
    """Full install (with service) runs 'peppy repo update' after the daemon starts."""
    home = peppy_home("service")

    result = login_shell(install_cmd(home, skip_service_install=False), timeout=600)

    assert_installed(result)
    output = result.stdout + result.stderr
    assert (
        "Refreshing repositories" in output or "Repository refresh complete" in output
    ), f"Missing repo update output{diagnostic(result)}"


def test_reinstall_over_existing() -> None:
    """Reinstall succeeds over an existing installation and keeps PEPPY_HOME."""
    home = peppy_home("reinstall")

    assert_installed(login_shell(install_cmd(home)))
    marker = login_shell(f"echo marker > {home}/test_marker.txt")
    assert marker.returncode == 0, f"could not write the marker{diagnostic(marker)}"

    # Second install: overwrites the binaries in place.
    assert_installed(login_shell(install_cmd(home)))

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
    env_preamble = peppy_environment(home)

    assert_installed(
        login_shell(install_cmd(home, skip_service_install=False), timeout=600)
    )

    init = login_shell(
        f"{env_preamble} && cd {home} && peppy node init --container test-node "
        "'A node that the install test builds in a container.'"
    )
    assert init.returncode == 0, f"node init failed{diagnostic(init)}"

    # Builds the node's Apptainer container.
    add = login_shell(
        f"{env_preamble} && cd {home}/test-node && peppy node add . --build",
        timeout=900,
    )
    assert add.returncode == 0, f"node add failed{diagnostic(add)}"

    run = login_shell(f"{env_preamble} && peppy node run test-node:v1")
    assert run.returncode == 0, f"node run failed{diagnostic(run)}"


def test_quickstart(capsys: pytest.CaptureFixture[str]) -> None:
    """The steps of the quickstart, on the machine as install.sh leaves it.

    A full install: no variable of install.sh skips a step. The background
    service that it registers must then run, and `peppy info` must print its
    `Daemon Info` block, which is the answer of that service. On Linux, the
    launch of the quickstart then runs on that service: `peppy stack launch`
    returns once each node has built its container image, has started and has
    ended its setup. On macOS, peppy runs each node in a Linux virtual
    machine, and a macOS runner is a virtual machine that cannot start one,
    so the test ends before the launch there.

    The log of the run shows what each step printed. The test resets the
    stack it launched, so the tests after it share the machine with no node.
    """
    home = peppy_home("quickstart")
    environment = peppy_environment(home)
    pin_default_hubs(home)

    install = login_shell(install_cmd(home, skip_service_install=False), timeout=600)
    show_step(capsys, "install.sh", install)
    assert_installed(install)

    service = login_shell(SERVICE_STATUS_COMMAND)
    show_step(capsys, "The background service", service)
    assert service.returncode == 0 and SERVICE_RUNNING_MARK in service.stdout, (
        f"the background service should run after the install{diagnostic(service)}"
    )

    info = login_shell(f"{environment} && peppy info")
    show_step(capsys, "peppy info", info)
    assert info.returncode == 0 and "Daemon Info" in info.stdout, (
        f"`peppy info` should print the answer of the daemon{diagnostic(info)}"
    )

    if not IS_LINUX:
        return

    try:
        launch = login_shell(
            f"{environment} && {QUICKSTART_LAUNCH}",
            timeout=QUICKSTART_LAUNCH_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired as hung:
        pytest.fail(
            f"`{QUICKSTART_LAUNCH}` did not end within "
            f"{QUICKSTART_LAUNCH_TIMEOUT_SECONDS} seconds"
            f"{diagnostic(hung)}{daemon_logs(home)}"
        )
    show_step(capsys, QUICKSTART_LAUNCH, launch)
    assert launch.returncode == 0 and "Launch complete" in launch.stdout, (
        f"`{QUICKSTART_LAUNCH}` should exit with 0 and print `Launch complete`"
        f"{diagnostic(launch)}{daemon_logs(home)}"
    )

    reset = login_shell(f"{environment} && peppy stack reset", timeout=600)
    assert reset.returncode == 0, f"the stack reset failed{diagnostic(reset)}"
