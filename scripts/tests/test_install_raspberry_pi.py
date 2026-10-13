"""Tests for scripts/install.sh on Raspberry Pi boards.

Of the Raspberry Pi boards, install.sh installs on those built on the BCM2712
SoC only: the Raspberry Pi 5, the Pi 500 and the Compute Module 5. The rule
holds whatever the distribution, Raspberry Pi OS, Ubuntu or Debian.
Raspberry Pi OS runs on Raspberry Pi boards only, so install.sh also refuses
it where the device tree names no board. Of Raspberry Pi OS it installs the
64-bit release based on Debian 13 or later. It refuses the rest before it
changes anything.

Each test runs install.sh in a container, with the aarch64 release archive
and the device tree of the board the test stands for. Raspberry Pi OS is a
container of its official image (see raspberry_pi_os). The images are arm64,
so the tests run on an aarch64 Linux host only, a disposable one like every
install test (see conftest). The containers take the container path of
install.sh; the host path is the one test_install.py tests on Ubuntu.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

import pytest

from .install_helpers import (
    assert_binaries_load,
    assert_installed,
    assert_refused,
    binaries_load_script,
    container_install_cmd,
    diagnostic,
    host_file,
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
    "of the Raspberry Pi boards, peppy supports the Raspberry Pi 5, the Pi 500 "
    "and the Compute Module 5 only."
)

# The other distributions install.sh supports on a Raspberry Pi, as images
# of their own.
OTHER_DISTRIBUTIONS = ["ubuntu:24.04", "debian:trixie"]

# Raspberry Pi OS based on Debian 12 is Debian 12 with the /etc/rpi-issue of
# its image, and what install.sh reads of it (os-release, glibc, dpkg) is
# Debian's. So the tests stand for it with the Debian 12 image and this file.
DEBIAN_12_IMAGE = "debian:bookworm"
BOOKWORM_RPI_ISSUE = """\
Raspberry Pi reference 2026-10-06
Generated using pi-gen, https://github.com/RPi-Distro/pi-gen, stage2
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
    and the libraries of Debian.
    """
    result = run_in_container(
        raspberry_pi_os,
        binaries_load_script(),
        device_tree=device_tree(tmp_path / "device-tree", RASPBERRY_PI_5),
    )

    assert_binaries_load(result)


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
    """Raspberry Pi OS whose device tree names no board is not on a Raspberry Pi 5."""
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
        "peppy needs a 64-bit system (arm64 packages), and the packages of this "
        "system are 'armhf'.",
    )
    assert "Raspberry Pi OS (64-bit) based on Debian 13" in result.stderr, (
        f"error should name the supported release{diagnostic(result)}"
    )


def test_install_refuses_raspberry_pi_os_based_on_bookworm(tmp_path: Path) -> None:
    """install.sh stops on Raspberry Pi OS based on Debian 12, whose glibc is 2.36."""
    result = run_in_container(
        DEBIAN_12_IMAGE,
        refusal_script(),
        files={"/etc/rpi-issue": host_file(tmp_path / "rpi-issue", BOOKWORM_RPI_ISSUE)},
        device_tree=device_tree(tmp_path / "device-tree", RASPBERRY_PI_5),
    )

    assert_refused(
        result,
        "'Debian GNU/Linux 12 (bookworm)' has glibc 2.36, and peppy needs glibc "
        "2.38 or later.",
    )
    assert "Raspberry Pi OS (64-bit) based on Debian 13 (trixie) or later" in (
        result.stderr
    ), f"error should name the oldest supported release{diagnostic(result)}"


@pytest.mark.parametrize("image", OTHER_DISTRIBUTIONS)
def test_install_with_another_distribution_on_a_raspberry_pi_5(
    tmp_path: Path, image: str
) -> None:
    """Ubuntu and Debian install on a Raspberry Pi 5 as on any other machine."""
    result = run_in_container(
        image,
        container_install_cmd(),
        device_tree=device_tree(tmp_path / "device-tree", RASPBERRY_PI_5),
    )

    assert_installed(result)


@pytest.mark.parametrize("image", OTHER_DISTRIBUTIONS)
def test_install_refuses_another_distribution_on_a_raspberry_pi_4(
    tmp_path: Path, image: str
) -> None:
    """The board rule holds whatever the distribution."""
    result = run_in_container(
        image,
        refusal_script(),
        device_tree=device_tree(tmp_path / "device-tree", RASPBERRY_PI_4),
    )

    assert_refused(result, BOARD_NOT_SUPPORTED)
    assert f"This board is a '{RASPBERRY_PI_4.model}'." in result.stderr, (
        f"error should name the board{diagnostic(result)}"
    )
