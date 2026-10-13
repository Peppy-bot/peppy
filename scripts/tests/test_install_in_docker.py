"""Tests for scripts/install.sh inside Docker containers.

Two parts of install.sh are decided by the system it runs on rather than by
the machine's state, and a container is a system of its own for each test:

- The container path. Inside Docker or Podman install.sh skips D-Bus, linger,
  fuse2fs and the Apptainer setup, which the container runtime has no use for.
- The platform check. install.sh reads os-release before it changes anything
  and stops on any Linux that is not Ubuntu, built on it, or Raspberry Pi OS.
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
