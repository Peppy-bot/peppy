"""The Raspberry Pi OS the install.sh tests run install.sh on.

Raspberry Pi OS publishes no container image, so the tests make one from the
image Raspberry Pi Imager writes to a card: the root filesystem of the
official Raspberry Pi OS Lite (64-bit), imported into Docker. The image is
pinned by its URL and SHA-256, so the tests run on the one release they are
written against until the pin moves.

The board is not part of the image: each test gives its container the device
tree of the board it stands for (see run_in_container).

Making the image needs root for a loop mount, Docker, and about 6 GB of
disk, which the disposable hosts of the install tests have. Docker keeps the
result under DOCKER_IMAGE, so a later session on the same host makes nothing.
"""

from __future__ import annotations

import hashlib
import shlex
import subprocess
from dataclasses import dataclass
from pathlib import Path

import pytest

from .install_helpers import TEST_ROOT, diagnostic, run_as_root

IMAGE_URL = (
    "https://downloads.raspberrypi.com/raspios_lite_arm64/images/"
    "raspios_lite_arm64-2026-10-06/2026-10-06-raspios-trixie-arm64-lite.img.xz"
)
IMAGE_SHA256 = "483db18a48da399b5b7022ffae9a07bc6d99daab30e593aedac09b69ff422843"
DOCKER_IMAGE = "peppy-install-test/raspberry-pi-os:2026-10-06-trixie-arm64-lite"

WORK_DIR = TEST_ROOT / "raspberry-pi-os"

_SECTOR_SIZE = 512
_MBR_SIGNATURE = b"\x55\xaa"
_MBR_PARTITION_TABLE_OFFSET = 446
_MBR_PARTITION_ENTRY_SIZE = 16
_LINUX_PARTITION_TYPE = 0x83


@dataclass(frozen=True)
class Partition:
    """Where a partition lies in a disk image, in bytes."""

    offset: int
    size: int


def linux_partition(disk: Path) -> Partition:
    """The one Linux partition of *disk*, read from its MBR partition table.

    A Raspberry Pi OS image holds two partitions: the FAT boot partition the
    firmware reads, then the root filesystem.
    """
    with disk.open("rb") as image:
        mbr = image.read(_SECTOR_SIZE)
    if mbr[-len(_MBR_SIGNATURE) :] != _MBR_SIGNATURE:
        pytest.fail(f"{disk} has no MBR partition table")
    partitions: list[Partition] = []
    for index in range(4):
        start = _MBR_PARTITION_TABLE_OFFSET + index * _MBR_PARTITION_ENTRY_SIZE
        entry = mbr[start : start + _MBR_PARTITION_ENTRY_SIZE]
        if entry[4] != _LINUX_PARTITION_TYPE:
            continue
        first_sector = int.from_bytes(entry[8:12], "little")
        sector_count = int.from_bytes(entry[12:16], "little")
        partitions.append(
            Partition(first_sector * _SECTOR_SIZE, sector_count * _SECTOR_SIZE)
        )
    if len(partitions) != 1:
        pytest.fail(f"{disk} should hold one Linux partition, found {len(partitions)}")
    return partitions[0]


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def _run(command: list[str], *, timeout: int) -> None:
    result = subprocess.run(
        command,
        capture_output=True,
        text=True,
        timeout=timeout,
        stdin=subprocess.DEVNULL,
        check=False,
    )
    if result.returncode != 0:
        pytest.fail(f"{shlex.join(command)} failed{diagnostic(result)}")


def _has_docker_image(tag: str) -> bool:
    return (
        subprocess.run(
            ["docker", "image", "inspect", tag],
            capture_output=True,
            stdin=subprocess.DEVNULL,
            check=False,
        ).returncode
        == 0
    )


def _download_disk_image() -> Path:
    """Download the pinned image, check its SHA-256, and decompress it."""
    WORK_DIR.mkdir(parents=True, exist_ok=True)
    compressed = WORK_DIR / IMAGE_URL.rsplit("/", 1)[1]
    _run(
        [
            "curl",
            "-fsSL",
            "--retry",
            "3",
            "--retry-connrefused",
            "--output",
            str(compressed),
            IMAGE_URL,
        ],
        timeout=1800,
    )
    digest = _sha256(compressed)
    if digest != IMAGE_SHA256:
        compressed.unlink()
        pytest.fail(
            f"{IMAGE_URL} has SHA-256 {digest}, and the tests pin {IMAGE_SHA256}"
        )
    # Decompresses in place and removes the .xz.
    _run(
        ["xz", "--decompress", "--force", "--threads=0", str(compressed)], timeout=1800
    )
    return compressed.with_suffix("")


def docker_image() -> str:
    """The tag of the Raspberry Pi OS image in Docker, made on first use."""
    if _has_docker_image(DOCKER_IMAGE):
        return DOCKER_IMAGE

    disk = _download_disk_image()
    root = linux_partition(disk)
    tag = shlex.quote(DOCKER_IMAGE)
    # sh has no pipefail, so tar writes down its own failure: a tar that stops
    # part way hands docker import a short archive, which imports without an
    # error.
    run_as_root(
        f"""
set -eu
mnt="$(mktemp -d)"
tar_failed="$(mktemp)"
mount -o loop,ro,offset={root.offset},sizelimit={root.size} {shlex.quote(str(disk))} "$mnt"
trap 'umount "$mnt"; rmdir "$mnt"; rm -f "$tar_failed"' EXIT
{{ tar --numeric-owner -C "$mnt" -cf - . || echo failed >"$tar_failed"; }} \\
    | docker image import --platform linux/arm64 - {tag}
if [ -s "$tar_failed" ]; then
    docker image rm {tag}
    exit 1
fi
""",
        timeout=1800,
    )
    disk.unlink()
    return DOCKER_IMAGE
