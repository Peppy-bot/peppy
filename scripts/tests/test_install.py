"""Tests for scripts/install.sh."""

from __future__ import annotations

import io
import subprocess
import tarfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
INSTALL_SCRIPT = REPO_ROOT / "scripts" / "install.sh"


def _create_fake_archive(directory: Path) -> Path:
    """Create a minimal .tgz archive that satisfies install.sh.

    The archive mirrors the layout produced by ``package_release()``:
    ``./bin/peppy`` and ``./bin/zenohd`` as executable shell stubs.
    """
    archive_path = directory / "peppy-fake.tgz"

    peppy_stub = b"#!/bin/sh\necho peppy-stub\n"
    zenohd_stub = b"#!/bin/sh\necho zenohd-stub\n"

    with tarfile.open(archive_path, "w:gz") as tar:
        for name, content in [
            ("./bin/peppy", peppy_stub),
            ("./bin/zenohd", zenohd_stub),
        ]:
            info = tarfile.TarInfo(name=name)
            info.size = len(content)
            info.mode = 0o755
            tar.addfile(info, io.BytesIO(content))

    return archive_path


def test_install(tmp_path: Path) -> None:
    """Run install.sh inside an ubuntu:24.04 container and verify success."""
    _create_fake_archive(tmp_path)

    # The shell commands run inside the container:
    # 1. Run install.sh with the local archive (skips download).
    # 2. Verify the installed binary is executable.
    # 3. Verify the installed binary actually runs.
    container_script = (
        "sh /mnt/install.sh /mnt/archive/peppy-fake.tgz"
        " && test -x /root/.peppy/bin/peppy"
        " && /root/.peppy/bin/peppy"
    )

    result = subprocess.run(
        [
            "docker",
            "run",
            "--rm",
            "-v",
            f"{INSTALL_SCRIPT}:/mnt/install.sh:ro",
            "-v",
            f"{tmp_path}:/mnt/archive:ro",
            "ubuntu:24.04",
            "sh",
            "-c",
            container_script,
        ],
        capture_output=True,
        text=True,
        timeout=120,
    )

    diagnostic = f"\n--- stdout ---\n{result.stdout}\n--- stderr ---\n{result.stderr}"

    assert result.returncode == 0, (
        f"install.sh exited with {result.returncode}{diagnostic}"
    )
    assert "peppy installed to" in result.stdout, (
        f"Missing 'peppy installed to' in output{diagnostic}"
    )
    assert "peppy is ready" in result.stdout, (
        f"Missing 'peppy is ready' in output{diagnostic}"
    )
