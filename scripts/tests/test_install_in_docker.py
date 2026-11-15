"""Tests for scripts/install.sh inside Docker containers.

Two parts of install.sh are decided by the system it runs on rather than by
the machine's state, and a container is a system of its own for each test:

- The container path. Inside Docker or Podman install.sh skips D-Bus and
  linger, which the container runtime has no use for. Its Apptainer setup
  runs as on a host: it installs the packages that container nodes need, and
  a bare image has none of them.
- The platform check. install.sh reads os-release before it changes anything
  and stops on any Linux that is not Ubuntu, built on it, Debian or Raspberry
  Pi OS, on a 32-bit system, and on a glibc older than the release archives
  need. The cases of Raspberry Pi boards need arm64 images, and are in
  test_install_raspberry_pi.py.

Each container gets install.sh and the release archive of this machine's
platform read-only, and nothing else of the host. The tests need Docker and
the archive, so like every install test they run only on a disposable host
(see conftest); on macOS there is no Docker to run them with.
"""

from __future__ import annotations

import shlex
from pathlib import Path

import pytest

from .install_helpers import (
    CONTAINER_PEPPY,
    CONTAINER_PEPPY_HOME,
    IS_LINUX_AARCH64,
    assert_binaries_load,
    assert_installed,
    assert_refused,
    binaries_load_script,
    container_install_cmd,
    diagnostic,
    host_file,
    linux_only,
    refusal_script,
    run_in_container,
)

pytestmark = [pytest.mark.install, linux_only]

UBUNTU_IMAGE = "ubuntu:24.04"
# The newest Ubuntu release whose glibc (2.35) is older than the 2.38 the
# release archives need.
UBUNTU_TOO_OLD_IMAGE = "ubuntu:22.04"
DEBIAN_IMAGE = "debian:trixie"
# The newest Debian release whose glibc (2.36) is too old.
DEBIAN_TOO_OLD_IMAGE = "debian:bookworm"

# The architecture of the packages of a 32-bit system that a 64-bit kernel of
# this machine's architecture runs.
THIRTY_TWO_BIT_PACKAGES = "armhf" if IS_LINUX_AARCH64 else "i386"


# Divides the output of install.sh from the output of the `peppy container
# status` that a script runs after it.
STATUS_MARK = "--- peppy container status ---"

# The apt-get of the fix script of `peppy container setup`.
SETUP_APT_GET = "apt-get -o DPkg::Lock::Timeout=300"


def status_line(program: str, state: str) -> str:
    """The line of `peppy container status` for *program* in *state*."""
    return f"  {program:<23}: {state}"


@pytest.mark.parametrize("image", [UBUNTU_IMAGE, DEBIAN_IMAGE])
def test_install_inside_container_installs_the_packages_of_container_nodes(
    image: str,
) -> None:
    """In a bare container, as root, install.sh installs the packages that container nodes need.

    Root builds with no fakeroot, so the setup installs squashfs-tools and
    fuse2fs, and not uidmap. Each check of `peppy container status` then
    passes.
    """
    script = (
        f"{container_install_cmd(container_setup=True)} && "
        f"echo '{STATUS_MARK}' && {CONTAINER_PEPPY} container status && "
        "command -v mksquashfs && command -v fuse2fs && ! command -v newuidmap"
    )

    result = run_in_container(image, script)

    assert_installed(result)
    install_output, _, status_output = result.stdout.partition(STATUS_MARK)
    for package in ("squashfs-tools", "fuse2fs"):
        assert f"Install the {package} package" in install_output, (
            f"the setup should tell that it installs {package}{diagnostic(result)}"
        )
    assert "Install the uidmap package" not in install_output, (
        f"root does not need uidmap{diagnostic(result)}"
    )
    assert "(requires sudo)" not in install_output and "sudo: " not in result.stderr, (
        f"root installs the packages with no sudo{diagnostic(result)}"
    )
    assert (
        status_line("newuidmap", "not required (the builds of root do not run it)")
        in status_output
    ), f"the status should not ask root for newuidmap{diagnostic(result)}"
    assert "All checks passed. Container support is ready." in status_output, (
        f"each check should pass after the install{diagnostic(result)}"
    )


def test_container_setup_has_nothing_to_do_after_the_install() -> None:
    """After install.sh, `peppy container setup` has no fix left to make.

    A container on an Ubuntu 24.04 host reads the host kernel's
    apparmor_restrict_unprivileged_userns, but has no AppArmor security
    filesystem to load a profile through: the setup leaves AppArmor alone
    where it cannot manage it.
    """
    script = (
        f"{container_install_cmd(container_setup=True)} >/dev/null && "
        f"{CONTAINER_PEPPY} container setup"
    )

    result = run_in_container(UBUNTU_IMAGE, script)

    assert result.returncode == 0, f"peppy container setup failed{diagnostic(result)}"
    assert "AppArmor is not manageable on this system" in result.stdout, (
        f"setup should detect the missing AppArmor security filesystem{diagnostic(result)}"
    )
    assert "Nothing to do." in result.stdout, (
        f"Expected 'Nothing to do.'{diagnostic(result)}"
    )


def test_install_without_the_container_setup_leaves_the_container_as_it_is() -> None:
    """With PEPPY_NO_CONTAINER_SETUP, install.sh installs peppy and no package."""
    script = (
        f"{container_install_cmd()} && ! command -v mksquashfs && ! command -v fuse2fs"
    )

    result = run_in_container(UBUNTU_IMAGE, script)

    assert_installed(result)
    assert (
        "Skipped Apptainer setup (PEPPY_NO_CONTAINER_SETUP is set)." in result.stdout
    ), f"Missing the skip message{diagnostic(result)}"
    assert "The following fixes are needed" not in result.stdout, (
        f"install.sh should not run the setup{diagnostic(result)}"
    )


def test_no_root_install_names_the_missing_packages_that_the_daemon_needs() -> None:
    """With PEPPY_NO_ROOT_INSTALL, install.sh installs no package and stops on those that the daemon needs.

    A bare image has none of the packages. The daemon of root does not start
    without squashfs-tools, and starts without fuse2fs. The error names the
    first one, and gives the command that installs the two.
    """
    install = container_install_cmd(
        container_setup=True, extra_env="PEPPY_NO_ROOT_INSTALL=1"
    )
    script = (
        f"{install}; status=$?; "
        "! command -v mksquashfs || echo 'a package was installed'; "
        "exit $status"
    )

    result = run_in_container(UBUNTU_IMAGE, script)

    output = result.stdout + result.stderr
    assert result.returncode == 1, (
        f"install.sh should stop with status 1{diagnostic(result)}"
    )
    for line in (
        status_line("newuidmap", "not required (the builds of root do not run it)"),
        status_line("mksquashfs", "FAILED (install the squashfs-tools package)"),
        status_line(
            "fuse2fs",
            "WARNING (install the fuse2fs package for the mount of an EXT3 image)",
        ),
        "The daemon does not start without packages that this machine does not "
        "have: squashfs-tools.",
        f"{SETUP_APT_GET} update \\\n"
        f"  && {SETUP_APT_GET} install -y squashfs-tools fuse2fs",
        "error: peppy is installed, but its daemon cannot start without these packages.",
    ):
        assert line in output, f"the output should hold {line!r}{diagnostic(result)}"
    assert "a package was installed" not in result.stdout, (
        f"install.sh should install no package{diagnostic(result)}"
    )


def test_install_as_a_user_that_is_not_root_installs_the_packages_through_sudo() -> (
    None
):
    """In a container, install.sh gives a user that is not root the three packages, through sudo.

    The builds of a user that is not root run newuidmap, so the setup
    installs uidmap also. The kernel of the host decides whether a container
    gives such a user a user namespace, so the test reads the lines of the
    packages in `peppy container status`, and not its exit status.
    """
    user = "peppy-user"
    as_root = (
        "apt-get update -qq && apt-get install -y -qq sudo >/dev/null && "
        f"useradd --create-home {user} && "
        f"echo '{user} ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/{user} && "
        f"install -d -o {user} -g {user} {CONTAINER_PEPPY_HOME}"
    )
    as_user = (
        f"{container_install_cmd(container_setup=True)} && "
        f"echo '{STATUS_MARK}' && {{ {CONTAINER_PEPPY} container status; true; }}"
    )

    result = run_in_container(
        DEBIAN_IMAGE, f"{as_root} && su {user} -c {shlex.quote(as_user)}"
    )

    assert_installed(result)
    install_output, _, status_output = result.stdout.partition(STATUS_MARK)
    for package in ("uidmap", "squashfs-tools", "fuse2fs"):
        assert f"Install the {package} package" in install_output, (
            f"the setup should tell that it installs {package}{diagnostic(result)}"
        )
    for program in ("newuidmap", "mksquashfs", "fuse2fs"):
        assert status_line(program, "OK") in status_output, (
            f"the status should find {program}{diagnostic(result)}"
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
        ("alpine:latest", "Alpine Linux"),
    ],
)
def test_install_refuses_unsupported_distribution(image: str, name: str) -> None:
    """install.sh stops on a Linux that is neither Ubuntu, built on it, nor
    Debian, before it changes anything."""
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


def test_every_installed_binary_loads_on_debian_13() -> None:
    """install.sh installs on Debian 13, whose glibc and libraries every binary finds."""
    result = run_in_container(DEBIAN_IMAGE, binaries_load_script())

    assert_binaries_load(result)


def test_install_refuses_debian_12() -> None:
    """install.sh stops on Debian 12, whose glibc is 2.36."""
    result = run_in_container(DEBIAN_TOO_OLD_IMAGE, refusal_script())

    assert_refused(
        result,
        "'Debian GNU/Linux 12 (bookworm)' has glibc 2.36, and peppy needs glibc "
        "2.38 or later.",
    )
    assert "peppy supports Debian 13 (trixie) or later." in result.stderr, (
        f"error should name the oldest supported release{diagnostic(result)}"
    )


def test_install_refuses_a_distribution_only_like_debian(tmp_path: Path) -> None:
    """A distribution that names debian in ID_LIKE alone is not Debian.

    Some of them run without systemd, which the peppy service needs.
    """
    os_release = host_file(
        tmp_path / "os-release",
        'PRETTY_NAME="Devuan GNU/Linux 6 (excalibur)"\nNAME="Devuan GNU/Linux"\n'
        'VERSION_ID="6"\nID=devuan\nID_LIKE=debian\n',
    )

    result = run_in_container(
        DEBIAN_IMAGE, refusal_script(), files={"/etc/os-release": os_release}
    )

    assert_refused(
        result, "unsupported Linux distribution 'Devuan GNU/Linux 6 (excalibur)'."
    )


def test_install_refuses_a_32_bit_system(tmp_path: Path) -> None:
    """install.sh stops on 32-bit packages, which a 64-bit kernel can run.

    `uname -m` names the architecture of the kernel, so the container stands
    for such a system with a dpkg that names 32-bit packages.
    """
    dpkg = host_file(
        tmp_path / "dpkg",
        f'#!/bin/sh\n[ "$1" = --print-architecture ] && echo '
        f"{THIRTY_TWO_BIT_PACKAGES}\n",
        mode=0o755,
    )

    result = run_in_container(
        UBUNTU_IMAGE, refusal_script(), files={"/usr/bin/dpkg": dpkg}
    )

    assert_refused(result, "peppy needs a 64-bit system")
    assert f"the packages of this system are '{THIRTY_TWO_BIT_PACKAGES}'" in (
        result.stderr
    ), f"error should name the architecture of the packages{diagnostic(result)}"
