"""Tests for scripts/install.sh on Raspberry Pi OS.

install.sh installs on the 64-bit Raspberry Pi OS based on Debian 13 or
later, on the boards built on the BCM2712 SoC: the Raspberry Pi 5, the Pi 500
and the Compute Module 5. It refuses every other board, the 32-bit release,
and the releases based on an older Debian, before it changes anything.

Each test runs install.sh in a container of the official Raspberry Pi OS
image (see raspberry_pi_os), with the aarch64 release archive and the device
tree of the board the test stands for. The image is arm64, so the tests run
on an aarch64 Linux host only, a disposable one like every install test (see
conftest). The container takes the container path of install.sh; the host
path is the one test_install.py tests on Ubuntu.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

import pytest

from .install_helpers import (
    CONTAINER_PEPPY_HOME,
    assert_installed,
    assert_refused,
    container_install_cmd,
    diagnostic,
    linux_aarch64_only,
    refusal_script,
    run_in_container,
)
from .raspberry_pi_os import docker_image

pytestmark = [pytest.mark.install, linux_aarch64_only]


@dataclass(frozen=True)
class Board:
    """A board as its device tree names it: `model`, then `compatible`, the
    board and its SoC."""

    model: str
    compatible: tuple[str, ...]


RASPBERRY_PI_5 = Board(
    "Raspberry Pi 5 Model B Rev 1.0", ("raspberrypi,5-model-b", "brcm,bcm2712")
)
RASPBERRY_PI_500 = Board(
    "Raspberry Pi 500 Rev 1.0", ("raspberrypi,500", "brcm,bcm2712")
)
COMPUTE_MODULE_5 = Board(
    "Raspberry Pi Compute Module 5 Rev 1.0",
    ("raspberrypi,5-compute-module", "brcm,bcm2712"),
)
RASPBERRY_PI_4 = Board(
    "Raspberry Pi 4 Model B Rev 1.5", ("raspberrypi,4-model-b", "brcm,bcm2711")
)
RASPBERRY_PI_3 = Board(
    "Raspberry Pi 3 Model B Plus Rev 1.3",
    ("raspberrypi,3-model-b-plus", "brcm,bcm2837"),
)

BOARD_NOT_SUPPORTED = (
    "peppy supports Raspberry Pi OS on a Raspberry Pi 5, a Pi 500 or a "
    "Compute Module 5 only."
)

# The os-release of Raspberry Pi OS based on Debian 12, which is Debian's own.
BOOKWORM_OS_RELEASE = """\
PRETTY_NAME="Debian GNU/Linux 12 (bookworm)"
NAME="Debian GNU/Linux"
VERSION_ID="12"
VERSION="12 (bookworm)"
VERSION_CODENAME=bookworm
ID=debian
HOME_URL="https://www.debian.org/"
SUPPORT_URL="https://www.debian.org/support"
BUG_REPORT_URL="https://bugs.debian.org/"
"""

# What the dpkg of the 32-bit Raspberry Pi OS answers install.sh. On a
# Raspberry Pi 5 that release also runs a 64-bit kernel, so only the
# architecture of its packages tells it apart.
ARMHF_DPKG = """\
#!/bin/sh
[ "$1" = --print-architecture ] && echo armhf
"""


@pytest.fixture(scope="module")
def raspberry_pi_os() -> str:
    """The Docker image of Raspberry Pi OS, made once for the module."""
    return docker_image()


def device_tree(directory: Path, board: Board) -> Path:
    """Write the device tree of *board* into *directory*, the files and
    the NUL-terminated strings the kernel shows."""
    directory.mkdir(mode=0o755)
    (directory / "model").write_bytes(board.model.encode() + b"\0")
    (directory / "compatible").write_bytes(
        b"".join(name.encode() + b"\0" for name in board.compatible)
    )
    for entry in directory.iterdir():
        entry.chmod(0o644)
    return directory


def host_file(path: Path, content: str, *, mode: int = 0o644) -> Path:
    """Write a file a container gets in place of one of its own."""
    path.write_text(content)
    path.chmod(mode)
    return path


@pytest.mark.parametrize(
    "board",
    [RASPBERRY_PI_5, RASPBERRY_PI_500, COMPUTE_MODULE_5],
    ids=["raspberry-pi-5", "raspberry-pi-500", "compute-module-5"],
)
def test_install_on_a_bcm2712_board(
    raspberry_pi_os: str, tmp_path: Path, board: Board
) -> None:
    """install.sh installs peppy on Raspberry Pi OS on each BCM2712 board."""
    result = run_in_container(
        raspberry_pi_os,
        container_install_cmd(),
        device_tree=device_tree(tmp_path / "device-tree", board),
    )

    assert_installed(result)


def test_every_installed_binary_loads_on_raspberry_pi_os(
    raspberry_pi_os: str, tmp_path: Path
) -> None:
    """Every binary of the archive finds its libraries and glibc on Raspberry Pi OS.

    The archive is built on Ubuntu 24.04, and Raspberry Pi OS has the glibc
    and the libraries of Debian. ldd reports each library or glibc version a
    binary needs and the system does not have, including for the binaries
    only a running container executes (apptainer's starter), and the three
    commands a user runs first must start.
    """
    bin_dir = f"{CONTAINER_PEPPY_HOME}/bin"
    script = (
        f"{container_install_cmd()} >/dev/null && "
        f"find {bin_dir} -type f \\( -perm -u+x -o -name '*.so*' \\) "
        "-exec ldd {} + 2>&1; "
        f"{bin_dir}/peppy --version && "
        f"{bin_dir}/zenohd --version && "
        f"{bin_dir}/apptainer/bin/apptainer --version"
    )

    result = run_in_container(
        raspberry_pi_os,
        script,
        device_tree=device_tree(tmp_path / "device-tree", RASPBERRY_PI_5),
    )

    assert "not found" not in result.stdout, (
        f"a binary needs a library or glibc version that Raspberry Pi OS does "
        f"not have{diagnostic(result)}"
    )
    assert result.returncode == 0, (
        f"peppy, zenohd and apptainer should start{diagnostic(result)}"
    )
    assert "apptainer version" in result.stdout, (
        f"apptainer should report its version{diagnostic(result)}"
    )


@pytest.mark.parametrize(
    "board", [RASPBERRY_PI_4, RASPBERRY_PI_3], ids=["raspberry-pi-4", "raspberry-pi-3"]
)
def test_install_refuses_a_board_older_than_the_raspberry_pi_5(
    raspberry_pi_os: str, tmp_path: Path, board: Board
) -> None:
    """install.sh stops on a board without the BCM2712, and names the board."""
    result = run_in_container(
        raspberry_pi_os,
        refusal_script(),
        device_tree=device_tree(tmp_path / "device-tree", board),
    )

    assert_refused(result, BOARD_NOT_SUPPORTED)
    assert f"This board is a '{board.model}'." in result.stderr, (
        f"error should name the board{diagnostic(result)}"
    )


def test_install_refuses_raspberry_pi_os_without_a_device_tree(
    raspberry_pi_os: str,
) -> None:
    """With no device tree to read, the board is not known to be a Raspberry Pi 5."""
    result = run_in_container(raspberry_pi_os, refusal_script())

    assert_refused(result, BOARD_NOT_SUPPORTED)
    assert "names no board" in result.stderr, (
        f"error should say the device tree names no board{diagnostic(result)}"
    )


def test_install_refuses_the_32_bit_raspberry_pi_os(
    raspberry_pi_os: str, tmp_path: Path
) -> None:
    """install.sh stops on the 32-bit release, whose kernel is 64-bit on a Pi 5."""
    result = run_in_container(
        raspberry_pi_os,
        refusal_script(),
        files={"/usr/bin/dpkg": host_file(tmp_path / "dpkg", ARMHF_DPKG, mode=0o755)},
        device_tree=device_tree(tmp_path / "device-tree", RASPBERRY_PI_5),
    )

    assert_refused(
        result,
        "peppy needs the 64-bit Raspberry Pi OS (arm64), and this system runs 'armhf'.",
    )


def test_install_refuses_raspberry_pi_os_based_on_bookworm(
    raspberry_pi_os: str, tmp_path: Path
) -> None:
    """install.sh stops on Raspberry Pi OS based on Debian 12, whose glibc is too old."""
    result = run_in_container(
        raspberry_pi_os,
        refusal_script(),
        files={
            "/etc/os-release": host_file(tmp_path / "os-release", BOOKWORM_OS_RELEASE)
        },
        device_tree=device_tree(tmp_path / "device-tree", RASPBERRY_PI_5),
    )

    assert_refused(
        result,
        "unsupported Raspberry Pi OS release, based on 'Debian GNU/Linux 12 (bookworm)'.",
    )
    assert "Debian 13 (trixie) or later" in result.stderr, (
        f"error should name the oldest supported release{diagnostic(result)}"
    )


def test_install_refuses_debian_on_a_raspberry_pi_5(
    raspberry_pi_os: str, tmp_path: Path
) -> None:
    """Without /etc/rpi-issue the system is Debian, which install.sh refuses on any board."""
    result = run_in_container(
        raspberry_pi_os,
        refusal_script(before="rm -f /etc/rpi-issue; "),
        device_tree=device_tree(tmp_path / "device-tree", RASPBERRY_PI_5),
    )

    assert_refused(
        result, "unsupported Linux distribution 'Debian GNU/Linux 13 (trixie)'."
    )
