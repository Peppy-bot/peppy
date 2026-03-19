"""Tests for scripts/install.sh."""

from __future__ import annotations

import subprocess
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent


def test_install_skips_loginctl_without_systemd() -> None:
    """install.sh must not queue loginctl enable-linger when loginctl is absent."""
    install_sh = REPO_ROOT / "scripts" / "install.sh"

    result = subprocess.run(
        [
            "docker", "run", "--rm",
            "-v", f"{install_sh}:/install.sh:ro",
            "ubuntu:24.04",
            "sh", "/install.sh",
        ],
        capture_output=True,
        text=True,
        timeout=120,
    )

    combined = result.stdout + result.stderr
    assert "Enable lingering" not in combined, (
        "install.sh queued 'loginctl enable-linger' despite loginctl being absent.\n"
        f"Output:\n{combined}"
    )
    assert "sudo: not found" not in combined, (
        "install.sh called sudo despite running as root.\n"
        f"Output:\n{combined}"
    )
    assert "you need either 'curl' or 'wget'" not in combined, (
        "install.sh failed to auto-install curl.\n"
        f"Output:\n{combined}"
    )
