"""Tests for scripts/install.sh inside Docker containers.

Two parts of install.sh are decided by the system it runs on rather than by
the machine's state, and a container is a system of its own for each test:

- The container path. Inside Docker or Podman install.sh skips D-Bus, linger,
  fuse2fs and the Apptainer setup, which the container runtime has no use for.
- The platform check. install.sh reads os-release before it changes anything
  and stops on any Linux that is not Ubuntu or built on it.

Each container gets install.sh and the release archive of this machine's
platform read-only, and nothing else of the host. The tests need Docker and
the archive, so like every install test they run only on a disposable host
(see conftest); on macOS there is no Docker to run them with.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from .install_helpers import INSTALL_SCRIPT, diagnostic, linux_only, release_archive

pytestmark = [pytest.mark.install, linux_only]

UBUNTU_IMAGE = "ubuntu:24.04"

# Where each container sees the two files it is given, and where it installs.
_CONTAINER_INSTALL_SCRIPT = "/peppy-test/install.sh"
_CONTAINER_PEPPY_HOME = "/peppy-home"


def _container_install_cmd() -> str:
    """install.sh as a container user runs it: no service, since no systemd runs."""
    archive = f"/peppy-test/{release_archive().name}"
    return (
        f"PEPPY_HOME={_CONTAINER_PEPPY_HOME} PEPPY_NO_SERVICE_INSTALL=1 "
        f"sh {_CONTAINER_INSTALL_SCRIPT} {archive}"
    )


def _run_in_container(
    image: str,
    script: str,
    *,
    os_release: Path | None = None,
    timeout: int = 600,
) -> subprocess.CompletedProcess[str]:
    """Run *script* with sh in a fresh container of *image*.

    *os_release* replaces the image's /etc/os-release, to present install.sh
    with a distribution the tests have no image of.
    """
    archive = release_archive()
    command = [
        "docker",
        "run",
        "--rm",
        "-v",
        f"{INSTALL_SCRIPT}:{_CONTAINER_INSTALL_SCRIPT}:ro",
        "-v",
        f"{archive}:/peppy-test/{archive.name}:ro",
    ]
    if os_release is not None:
        command += ["-v", f"{os_release}:/etc/os-release:ro"]
    command += [image, "sh", "-c", script]
    return subprocess.run(
        command,
        capture_output=True,
        text=True,
        timeout=timeout,
        stdin=subprocess.DEVNULL,
    )


# Prints a line the tests look for when install.sh left a PEPPY_HOME behind,
# after passing install.sh's own exit status through.
def _refusal_script(before: str = "") -> str:
    return (
        f"{before}{_container_install_cmd()}; status=$?; "
        f'[ ! -e {_CONTAINER_PEPPY_HOME} ] || echo "PEPPY_HOME was created"; '
        "exit $status"
    )


def _assert_refused(result: subprocess.CompletedProcess[str], message: str) -> None:
    assert result.returncode == 1, (
        f"install.sh should stop with status 1{diagnostic(result)}"
    )
    assert message in result.stderr, f"error should say {message!r}{diagnostic(result)}"
    assert "PEPPY_HOME was created" not in result.stdout, (
        f"install.sh should stop before it changes anything{diagnostic(result)}"
    )


def test_install_inside_container() -> None:
    """Inside a container install.sh installs peppy and skips the host setup."""
    result = _run_in_container(UBUNTU_IMAGE, _container_install_cmd())

    assert result.returncode == 0, (
        f"install.sh exited with {result.returncode}{diagnostic(result)}"
    )
    assert "peppy installed to" in result.stdout, (
        f"Missing 'peppy installed to' in the output{diagnostic(result)}"
    )
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
        f"{_container_install_cmd()} && "
        f"PEPPY_HOME={_CONTAINER_PEPPY_HOME} {_CONTAINER_PEPPY_HOME}/bin/peppy container setup"
    )

    result = _run_in_container(UBUNTU_IMAGE, script)

    assert result.returncode == 0, (
        f"peppy container setup failed{diagnostic(result)}"
    )
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
    result = _run_in_container(
        UBUNTU_IMAGE,
        f"grep -qx ID=linuxmint /etc/os-release && {_container_install_cmd()}",
        os_release=os_release,
    )

    assert result.returncode == 0, (
        f"install.sh exited with {result.returncode}{diagnostic(result)}"
    )
    assert "peppy installed to" in result.stdout, (
        f"Missing 'peppy installed to' in the output{diagnostic(result)}"
    )


def test_install_reads_usr_lib_os_release_without_etc_os_release() -> None:
    """With no /etc/os-release, install.sh identifies the system by /usr/lib/os-release."""
    script = (
        "rm -f /etc/os-release && [ -r /usr/lib/os-release ] && "
        f"{_container_install_cmd()}"
    )

    result = _run_in_container(UBUNTU_IMAGE, script)

    assert result.returncode == 0, (
        f"install.sh exited with {result.returncode}{diagnostic(result)}"
    )
    assert "peppy installed to" in result.stdout, (
        f"Missing 'peppy installed to' in the output{diagnostic(result)}"
    )


@pytest.mark.parametrize(
    ("image", "name"),
    [
        ("fedora:latest", "Fedora Linux"),
        ("debian:stable-slim", "Debian GNU/Linux"),
    ],
)
def test_install_refuses_unsupported_distribution(image: str, name: str) -> None:
    """install.sh stops on a Linux not built on Ubuntu, before it changes anything."""
    result = _run_in_container(image, _refusal_script())

    _assert_refused(result, "unsupported Linux distribution")
    assert name in result.stderr, (
        f"error should name the distribution{diagnostic(result)}"
    )


def test_install_refuses_without_os_release() -> None:
    """install.sh stops on a Linux it cannot identify, before it changes anything."""
    result = _run_in_container(
        UBUNTU_IMAGE,
        _refusal_script(before="rm -f /etc/os-release /usr/lib/os-release; "),
    )

    _assert_refused(result, "cannot identify this Linux distribution")
