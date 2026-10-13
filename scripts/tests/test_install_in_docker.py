"""Tests for scripts/install.sh inside Docker containers.

Two parts of install.sh are decided by the system it runs on rather than by
the machine's state, and a container is a system of its own for each test:

- The container path. Inside Docker or Podman install.sh skips D-Bus, linger,
  fuse2fs and the Apptainer setup, which the container runtime has no use for.
- The platform check. install.sh reads os-release before it changes anything
  and stops on any Linux that is not Ubuntu, built on it, or Raspberry Pi OS,
  and on a glibc older than the release archives need.
  The Raspberry Pi OS cases need its arm64 image and a Raspberry Pi board, and
  are in test_install_raspberry_pi_os.py.

Each container gets install.sh and the release archive of this machine's
platform read-only, and nothing else of the host. The tests need Docker and
the archive, so like every install test they run only on a disposable host
(see conftest); on macOS there is no Docker to run them with.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from .install_helpers import (
    CONTAINER_PEPPY_HOME,
    assert_installed,
    assert_refused,
    container_install_cmd,
    diagnostic,
    linux_only,
    refusal_script,
    run_in_container,
)

pytestmark = [pytest.mark.install, linux_only]

UBUNTU_IMAGE = "ubuntu:24.04"
# The newest Ubuntu release whose glibc (2.35) is older than the 2.38 the
# release archives need.
UBUNTU_TOO_OLD_IMAGE = "ubuntu:22.04"


def test_install_inside_container() -> None:
    """Inside a container install.sh installs peppy and skips the host setup."""
    result = run_in_container(UBUNTU_IMAGE, container_install_cmd())

    assert_installed(result)
    assert "Skipped Apptainer setup (running inside a container)." in result.stdout, (
        f"Missing the container skip message{diagnostic(result)}"
    )
    assert "peppy requires the following system changes" not in result.stdout, (
        f"install.sh should not change the container's system{diagnostic(result)}"
    )


def test_container_setup_has_nothing_to_do_inside_a_container() -> None:
    """`peppy container setup` leaves AppArmor alone where it cannot manage it.

    A container on an Ubuntu 24.04 host reads the host kernel's
    apparmor_restrict_unprivileged_userns, but has no AppArmor security
    filesystem to load a profile through. The container gets uidmap first, so
    AppArmor is the only thing left for the setup to decide on.
    """
    script = (
        "apt-get update -qq && apt-get install -y -qq uidmap >/dev/null && "
        f"{container_install_cmd()} && "
        f"PEPPY_HOME={CONTAINER_PEPPY_HOME} {CONTAINER_PEPPY_HOME}/bin/peppy container setup"
    )

    result = run_in_container(UBUNTU_IMAGE, script)

    assert result.returncode == 0, f"peppy container setup failed{diagnostic(result)}"
    assert "AppArmor is not manageable on this system" in result.stdout, (
        f"setup should detect the missing AppArmor security filesystem{diagnostic(result)}"
    )
    assert "Nothing to do." in result.stdout, (
        f"Expected 'Nothing to do.'{diagnostic(result)}"
    )


def test_install_admits_a_distribution_built_on_ubuntu(tmp_path: Path) -> None:
    """A distribution that names ubuntu in ID_LIKE installs like Ubuntu."""
    os_release = tmp_path / "os-release"
    os_release.write_text(
        'NAME="Linux Mint"\nID=linuxmint\nID_LIKE="ubuntu debian"\n'
        'PRETTY_NAME="Linux Mint 22"\n'
    )

    # The check in front proves the replacement is what install.sh reads.
    result = run_in_container(
        UBUNTU_IMAGE,
        f"grep -qx ID=linuxmint /etc/os-release && {container_install_cmd()}",
        files={"/etc/os-release": os_release},
    )

    assert_installed(result)


def test_install_reads_usr_lib_os_release_without_etc_os_release() -> None:
    """With no /etc/os-release, install.sh identifies the system by /usr/lib/os-release."""
    script = (
        "rm -f /etc/os-release && [ -r /usr/lib/os-release ] && "
        f"{container_install_cmd()}"
    )

    result = run_in_container(UBUNTU_IMAGE, script)

    assert_installed(result)


@pytest.mark.parametrize(
    ("image", "name"),
    [
        ("fedora:latest", "Fedora Linux"),
        ("debian:stable-slim", "Debian GNU/Linux"),
    ],
)
def test_install_refuses_unsupported_distribution(image: str, name: str) -> None:
    """install.sh stops on a Linux not built on Ubuntu, before it changes anything."""
    result = run_in_container(image, refusal_script())

    assert_refused(result, "unsupported Linux distribution")
    assert name in result.stderr, (
        f"error should name the distribution{diagnostic(result)}"
    )


def test_install_refuses_without_os_release() -> None:
    """install.sh stops on a Linux it cannot identify, before it changes anything."""
    result = run_in_container(
        UBUNTU_IMAGE,
        refusal_script(before="rm -f /etc/os-release /usr/lib/os-release; "),
    )

    assert_refused(result, "cannot identify this Linux distribution")


def test_install_refuses_ubuntu_22_04() -> None:
    """install.sh stops on a glibc older than 2.38, on which peppy cannot start."""
    result = run_in_container(UBUNTU_TOO_OLD_IMAGE, refusal_script())

    assert_refused(result, "has glibc 2.35, and peppy needs glibc 2.38 or later.")
    assert "'Ubuntu 22.04" in result.stderr, (
        f"error should name the distribution{diagnostic(result)}"
    )
    assert "Ubuntu 24.04 or later" in result.stderr, (
        f"error should name the oldest supported release{diagnostic(result)}"
    )


def test_install_refuses_a_distribution_built_on_ubuntu_22_04(tmp_path: Path) -> None:
    """A distribution built on Ubuntu 22.04 has its glibc, whatever its own version."""
    os_release = tmp_path / "os-release"
    os_release.write_text(
        'NAME="Linux Mint"\nVERSION_ID="21.3"\nID=linuxmint\n'
        'ID_LIKE="ubuntu debian"\nPRETTY_NAME="Linux Mint 21.3"\n'
    )

    result = run_in_container(
        UBUNTU_TOO_OLD_IMAGE,
        refusal_script(),
        files={"/etc/os-release": os_release},
    )

    assert_refused(
        result,
        "'Linux Mint 21.3' has glibc 2.35, and peppy needs glibc 2.38 or later.",
    )


def test_install_refuses_without_a_glibc_version() -> None:
    """install.sh stops when it cannot read the glibc version it checks."""
    result = run_in_container(
        UBUNTU_IMAGE, refusal_script(before="rm -f /usr/bin/getconf; ")
    )

    assert_refused(result, "cannot read the glibc version of this system")
